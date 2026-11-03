//! Tests for the headless `AgentSession` facade and the JSON-line RPC mode.
//!
//! The model is faked with a scripted provider (same pattern as
//! `tests/agent_loop.rs`); no `~/.pi` configuration is consulted.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use pir::agent::agent_loop::{AgentEvent, FinishTurnAction, FinishTurnContext};
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{
    agent_event_to_json, session_event_to_json, AgentSession, AgentSessionConfig, Disposition,
    SessionEvent, SessionEventHandler, SharedEventStreamFn,
};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::coding_agent::rpc::{RpcServer, SessionFactory};
use pir::extensions::{ExtensionConfig, ExtensionHandle, Notification};
use pir::plugins::{AgentPlugin, PluginEvent, PluginEventOutcome, PluginFuture, PluginInfo};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

/// Scripted model catalog entry.
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

fn tool_call(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: BTreeMap::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// Responder signature: (call index, request context) -> assistant message.
type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

/// A fake provider that answers every stream with a scripted message.
struct FakeProvider {
    calls: AtomicUsize,
    respond: Responder,
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
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
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn fake_registry(respond: Responder) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

fn registry_stream_override(registry: Arc<ProviderRegistry>, model: Model) -> SharedEventStreamFn {
    Arc::new(move |request| {
        // `ProviderRequest.context` is shared (`Arc<Context>`); the provider
        // trait takes an owned context, so materialize it through the loop's
        // own helper.
        let stream = registry.stream_unchecked(
            request.model.unwrap_or_else(|| model.clone()),
            pir::agent::agent_loop::context_into_owned(request.context),
            StreamOptions::default(),
        );
        Box::pin(async move { stream })
    })
}

/// A tool whose run blocks until the gate opens or the context aborts.
/// `entered` counts run starts so tests can await tool entry deterministically
/// (the loop only emits `tool_execution_start` after the batch completes).
#[derive(Clone)]
struct GateTool {
    gate: tokio::sync::watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
}

impl AgentTool for GateTool {
    fn name(&self) -> &'static str {
        "gate"
    }
    fn description(&self) -> &'static str {
        "blocks until released"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object"}))
    }
    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let mut gate = self.gate.clone();
        let aborted = ctx.signal.clone();
        Box::pin(async move {
            loop {
                if aborted.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    return ToolResult::error("aborted");
                }
                if *gate.borrow() {
                    return ToolResult::ok("gate open");
                }
                // Poll with a timeout so an abort while parked on the gate
                // watcher is still observed.
                if tokio::time::timeout(std::time::Duration::from_millis(20), gate.changed())
                    .await
                    .is_err()
                {
                    continue;
                }
            }
        })
    }
}

fn tool_registry_with(
    gate: Option<(tokio::sync::watch::Receiver<bool>, Arc<AtomicUsize>)>,
) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    if let Some((gate, entered)) = gate {
        tools.register(Arc::new(GateTool { gate, entered }));
    }
    tools
}

async fn wait_for_tool_entry(entered: &Arc<AtomicUsize>) {
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("gate tool never started");
}

/// Assert the request/transcript shape every provider requires: a tool result
/// answers the immediately preceding assistant's tool call, every tool call is
/// answered before the next assistant, and no result is left orphaned. This is
/// the invariant the length-stop recovery omission must preserve (upstream
/// #11227: an unpaired `toolResult` makes OpenAI/Anthropic reject every later
/// request).
fn assert_tool_pairs(messages: &[Message]) {
    let mut pending: Vec<String> = Vec::new();
    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                assert!(
                    pending.is_empty(),
                    "assistant after unanswered tool calls {pending:?}: {messages:?}"
                );
                pending = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => Some(call.id.clone()),
                        _ => None,
                    })
                    .collect();
            }
            Message::ToolResult(result) => {
                let position = pending
                    .iter()
                    .position(|id| *id == result.tool_call_id)
                    .unwrap_or_else(|| {
                        panic!(
                            "orphan tool result {:?} (pending {pending:?}): {messages:?}",
                            result.tool_call_id
                        )
                    });
                pending.remove(position);
            }
            _ => {}
        }
    }
    // A request may legitimately end on an assistant with calls only when the
    // transcript is being fed back for a tool round; the recovery retry is
    // built from a projection that never ends there, so require the list to be
    // fully answered for every request this suite validates.
    assert!(
        pending.is_empty(),
        "request ends with unanswered tool calls {pending:?}: {messages:?}"
    );
}

/// The stored entry ids of every assistant/tool result the recovery attempt
/// produced, in transcript order.
fn stored_attempt_entry_ids(branch: &[pir::session_manager::FileEntry]) -> Vec<String> {
    branch
        .iter()
        .filter_map(|entry| match entry {
            pir::session_manager::FileEntry::Message(message) => match &message.message {
                pir::session_manager::types::StoredMessage::Assistant(assistant)
                    if assistant.stop_reason == StopReason::Length
                        || assistant.stop_reason == StopReason::Error =>
                {
                    Some(message.id.clone())
                }
                pir::session_manager::types::StoredMessage::ToolResult(result)
                    if result.content.iter().any(|block| match block {
                        ContentBlock::Text(text) => text
                            .text
                            .contains("was not executed: the response hit the output token limit"),
                        _ => false,
                    }) =>
                {
                    Some(message.id.clone())
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Entry ids targeted by `replacement: null` context edits (model-visible
/// omissions).
fn omitted_entry_ids(branch: &[pir::session_manager::FileEntry]) -> Vec<String> {
    branch
        .iter()
        .filter_map(|entry| match entry {
            pir::session_manager::FileEntry::ContextEdit(edit) if edit.replacement.is_none() => {
                Some(edit.target_id.clone())
            }
            _ => None,
        })
        .collect()
}

/// Event recorder: (event type, JSON) pairs.
type EventLog = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

fn event_handler(log: EventLog) -> pir::agent_session::EventHandler {
    Arc::new(move |event: AgentEvent| {
        let json = agent_event_to_json(&event);
        let kind = json
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        log.lock().unwrap().push((kind, json));
    })
}

fn event_types(log: &EventLog) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .map(|(kind, _)| kind.clone())
        .collect()
}

/// Per-test temp session dir.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-agent-session-{label}-{}-{}",
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

fn text_of_message(message: &Message) -> Option<String> {
    match message {
        Message::User(user) => Some(
            user.content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        ),
        Message::Assistant(assistant) => Some(
            assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

fn user_texts(context: &Context) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(_) => text_of_message(message),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Facade tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prompt_round_trips_session_storage() {
    let temp = TempDir::new("round-trip");
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let path = session.path().to_path_buf();
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("Hello!")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(None),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let terminal = agent
        .prompt("hi", event_handler(events.clone()))
        .await
        .expect("prompt");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("Hello!")
    );

    // Reopen the session file and verify the transcript persisted.
    let reopened = SessionManager::open(&path).expect("reopen session");
    let entries = reopened.build_context_entries().expect("context entries");
    let rendered = entries
        .iter()
        .filter_map(text_of_message)
        .collect::<Vec<_>>();
    assert!(
        rendered.iter().any(|text| text == "hi"),
        "user message missing from storage: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|text| text == "Hello!"),
        "assistant message missing from storage: {rendered:?}"
    );

    // Lifecycle events flowed through. `StreamEnd` serializes as the
    // upstream `message_end` type (pinned event-name contract).
    let kinds = event_types(&events);
    assert_eq!(kinds.first().map(String::as_str), Some("agent_start"));
    assert_eq!(kinds.last().map(String::as_str), Some("agent_end"));
    assert!(kinds.contains(&"message_start".to_owned()));
    assert!(kinds.contains(&"message_end".to_owned()));
    let state = agent.state();
    assert_eq!(state.turns, 1);
    assert_eq!(state.model, "fake-model");
    assert_eq!(state.session_id.as_deref(), Some(reopened.id()));
}

/// The run-start storage work runs on the blocking pool: while the session's
/// next write is stalled, the async runtime keeps polling other tasks, and the
/// starting message still precedes the run's produced messages.
#[tokio::test]
async fn startup_append_runs_off_thread_and_keeps_message_order() {
    let temp = TempDir::new("startup-off-thread");
    let mut storage = SessionManager::create(".", &temp.0).expect("create session");
    // Long enough that a synchronous append would hold this single-threaded
    // runtime inside the stall for the whole probe window.
    storage.stall_next_append_for_test(std::time::Duration::from_millis(1_200));
    let path = storage.path().to_path_buf();
    let requests: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_responder = Arc::clone(&requests);
    let registry = fake_registry(Arc::new(move |_index, context| {
        requests_for_responder.lock().unwrap().push(context.clone());
        assistant(vec![text("Hello!")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(None),
        session: Some(storage),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move { agent.prompt("hi", Arc::new(|_| {})).await }
    });

    // Wait until the startup append OWNS the session lock (the stalled
    // blocking task). A zero-duration timeout polls the lock future exactly
    // once: `Elapsed` therefore means the mutex is held by the append while
    // this runtime task is running. A synchronous append would block the only
    // runtime thread inside the stall, so the loop could never observe the
    // held lock — the prompt would finish (or the loop exhaust) instead. No
    // wall-clock budget is involved, so a loaded runner cannot skew the probe.
    let mut observed_lock_held = false;
    for _ in 0..10_000 {
        assert!(
            !prompt_task.is_finished(),
            "the prompt completed before the stalled startup append was observed"
        );
        let probe = tokio::time::timeout(
            std::time::Duration::ZERO,
            agent.with_session_manager(|_| ()),
        )
        .await;
        if probe.is_err() {
            observed_lock_held = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        observed_lock_held,
        "the stalled startup append never held the session lock while the runtime was polling"
    );

    let terminal = prompt_task.await.expect("prompt task").expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);

    // The projection built after the append fed the first request, with the
    // starting message first.
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "one provider request");
    match requests[0].messages.first() {
        Some(Message::User(user)) => assert!(
            matches!(
                user.content.first(),
                Some(ContentBlock::Text(content)) if content.text == "hi"
            ),
            "the starting message leads the request: {:?}",
            requests[0].messages
        ),
        other => panic!("the starting user message is the first request message: {other:?}"),
    }

    // Storage order is unchanged: the starting user message, then the
    // produced assistant message.
    let reopened = SessionManager::open(&path).expect("reopen session");
    let messages = reopened.build_context_entries().expect("context entries");
    let roles: Vec<&str> = messages.iter().map(Message::role).collect();
    assert_eq!(
        roles,
        vec!["user", "assistant"],
        "the startup append still precedes the run's produced messages"
    );
    assert_eq!(text_of_message(&messages[0]).as_deref(), Some("hi"));
    assert_eq!(
        text_of_message(&messages[1]).as_deref(),
        Some("Hello!"),
        "the run's assistant message follows the starting one"
    );
}

#[tokio::test]
async fn steer_during_run_redirects_next_model_request() {
    let temp = TempDir::new("steer");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_for_responder = observed.clone();
    let registry = fake_registry(Arc::new(move |index, context| {
        if index == 0 {
            assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
        } else {
            observed_for_responder
                .lock()
                .unwrap()
                .extend(user_texts(context));
            assistant(vec![text("after steer")], StopReason::Stop)
        }
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(Some((gate_rx, entered.clone()))),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        let events = events.clone();
        async move { agent.prompt("start", event_handler(events)).await }
    });

    // Wait until the gate tool is executing, then steer and release it.
    wait_for_tool_entry(&entered).await;
    agent.steer("redirect now");
    let _ = gate_tx.send(true);
    let terminal = prompt_task
        .await
        .expect("prompt task")
        .expect("prompt result");

    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("after steer")
    );
    let seen = observed.lock().unwrap();
    assert!(
        seen.iter().any(|text| text == "redirect now"),
        "steering message never reached the model request: {seen:?}"
    );
}

#[tokio::test]
async fn abort_mid_run_stops_the_loop() {
    let temp = TempDir::new("abort");
    // The gate never opens; only aborting can end the tool.
    let (_gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(Some((gate_rx, entered.clone()))),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        let events = events.clone();
        async move { agent.prompt("start", event_handler(events)).await }
    });

    wait_for_tool_entry(&entered).await;
    agent.abort();
    let terminal = prompt_task
        .await
        .expect("prompt task")
        .expect("prompt result");

    // The loop ended on the abort signal after the tool batch, so the terminal
    // message is still the tool-use assistant message; the agent_end event
    // reports the abort.
    assert_eq!(terminal.stop_reason, StopReason::ToolUse);
    let agent_end = events
        .lock()
        .unwrap()
        .iter()
        .find(|(kind, _)| kind == "agent_end")
        .map(|(_, json)| json.clone())
        .expect("agent_end event");
    assert_eq!(agent_end["aborted"], serde_json::json!(true));
    assert!(!agent.is_streaming());
}

#[tokio::test]
async fn compaction_hook_invoked_and_persisted_over_threshold() {
    let temp = TempDir::new("compaction");
    let registry = fake_registry(Arc::new(|_index, context| {
        // The summarizer request is a single user message containing the
        // serialized conversation; anything else is a model turn.
        let is_summarizer = context.messages.len() == 1
            && matches!(&context.messages[0], Message::User(user)
            if user.content.iter().any(|block| matches!(
                block,
                ContentBlock::Text(text) if text.text.contains("<conversation>")
            )));
        if is_summarizer {
            assistant(vec![text("COMPACT SUMMARY")], StopReason::Stop)
        } else {
            assistant(vec![text("model reply")], StopReason::Stop)
        }
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let path = session.path().to_path_buf();
    // Tiny context window + reserve 1 => every context exceeds the budget.
    let mut model = fake_model();
    model.context_window = 64;
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model,
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(None),
        session: Some(session),
        compaction: Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 8,
        }),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    // Turn 1: only one user message; nothing precedes the cut point yet.
    agent
        .prompt(
            &format!("u-{}", "x".repeat(400)),
            event_handler(events.clone()),
        )
        .await
        .expect("first prompt");
    // Turn 2: [u1, a1, u2] -> trigger fires, [u1, a1] summarized.
    let terminal = agent
        .prompt(
            &format!("u-{}", "y".repeat(400)),
            event_handler(events.clone()),
        )
        .await
        .expect("second prompt");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("model reply")
    );

    let kinds = event_types(&events);
    assert!(
        kinds.contains(&"compaction".to_owned()),
        "compaction event missing: {kinds:?}"
    );

    // Reopen: the transcript restarts at the compaction entry.
    let reopened = SessionManager::open(&path).expect("reopen session");
    let entries = reopened.build_context_entries().expect("context entries");
    let rendered = entries
        .iter()
        .filter_map(text_of_message)
        .collect::<Vec<_>>();
    assert!(
        rendered.iter().any(|text| text.contains("COMPACT SUMMARY")),
        "compaction summary missing after reopen: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|text| text == "model reply"),
        "post-compaction assistant message missing: {rendered:?}"
    );
}

#[tokio::test]
async fn manual_compact_returns_summary_and_persists() {
    let temp = TempDir::new("manual-compact");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("MANUAL SUMMARY")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let path = session.path().to_path_buf();
    // A large window keeps the threshold hook idle: only the MANUAL compact
    // below must run (the upstream /compact errors with "nothing to compact"
    // when a compaction just happened and only the kept tail remains).
    let mut model = fake_model();
    model.context_window = 1_000_000;
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model,
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(None),
        session: Some(session),
        compaction: Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 8,
        }),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    // Seed a transcript with a complete turn before the cut point.
    let noop: EventLog = Arc::new(Mutex::new(Vec::new()));
    agent
        .prompt(
            &format!("u-{}", "x".repeat(400)),
            event_handler(noop.clone()),
        )
        .await
        .expect("seed prompt");
    agent
        .prompt(
            &format!("u-{}", "y".repeat(400)),
            event_handler(noop.clone()),
        )
        .await
        .expect("seed prompt 2");

    let result = agent.compact(None).await.expect("compact");
    assert_eq!(result.summary, "MANUAL SUMMARY");

    let reopened = SessionManager::open(&path).expect("reopen");
    let entries = reopened.build_context_entries().expect("entries");
    assert!(entries.iter().any(|message| matches!(
        message,
        Message::User(user) if user.content.iter().any(|block| matches!(
            block,
            ContentBlock::Text(text) if text.text.contains("MANUAL SUMMARY")
        ))
    )));
}

#[tokio::test]
async fn prompt_while_streaming_is_rejected() {
    let temp = TempDir::new("busy");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(Some((gate_rx, entered.clone()))),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let first = tokio::spawn({
        let agent = Arc::clone(&agent);
        let events = events.clone();
        async move { agent.prompt("start", event_handler(events)).await }
    });
    wait_for_tool_entry(&entered).await;

    // While the first prompt holds the gate open, a second prompt must be
    // rejected instead of interleaving runs.
    let second = agent
        .prompt("second", event_handler(Arc::new(Mutex::new(Vec::new()))))
        .await;
    assert!(second.is_err(), "concurrent prompt should be rejected");

    let _ = gate_tx.send(true);
    let terminal = first
        .await
        .expect("first prompt task")
        .expect("first prompt");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("done")
    );
}

#[tokio::test]
async fn ephemeral_session_keeps_history_across_prompts() {
    let temp = TempDir::new("ephemeral");
    let observed = Arc::new(Mutex::new(Vec::<usize>::new()));
    let observed_for_responder = observed.clone();
    let registry = fake_registry(Arc::new(move |_index, context| {
        observed_for_responder
            .lock()
            .unwrap()
            .push(context.messages.len());
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry.clone(),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(None),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: Some(registry_stream_override(registry, fake_model())),
        extensions: None,
        tool_selection: None,
    }));
    let noop: EventLog = Arc::new(Mutex::new(Vec::new()));
    agent
        .prompt("one", event_handler(noop.clone()))
        .await
        .expect("prompt one");
    agent
        .prompt("two", event_handler(noop.clone()))
        .await
        .expect("prompt two");
    let lengths = observed.lock().unwrap();
    // The second prompt's request must contain the first exchange.
    assert!(
        lengths.windows(2).any(|window| window[1] > window[0]),
        "ephemeral history not carried across prompts: {lengths:?}"
    );
}

// ---------------------------------------------------------------------------
// RPC protocol tests
// ---------------------------------------------------------------------------

fn rpc_factory() -> SessionFactory {
    Arc::new(|session: Option<SessionManager>| {
        let registry = fake_registry(Arc::new(|_index, _context| {
            assistant(vec![text("rpc reply")], StopReason::Stop)
        }));
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: tool_registry_with(None),
            session,
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: None,
            tool_selection: None,
        })))
    })
}

/// Factory whose run parks inside a `gate` tool on the first model turn
/// (shared `entered` counter), for prompt-race / writer-death tests.
fn rpc_factory_gated(
    gate_rx: tokio::sync::watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
) -> SessionFactory {
    Arc::new(move |session: Option<SessionManager>| {
        let gate_rx = gate_rx.clone();
        let registry = fake_registry(Arc::new(|index, _context| {
            if index == 0 {
                assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
            } else {
                assistant(vec![text("rpc reply")], StopReason::Stop)
            }
        }));
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: tool_registry_with(Some((gate_rx, Arc::clone(&entered)))),
            session,
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: None,
            tool_selection: None,
        })))
    })
}

/// Client half of an in-memory stdio pair: write JSON lines, read JSON lines.
struct RpcClient {
    writer: tokio::sync::mpsc::UnboundedSender<String>,
    reader: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl RpcClient {
    fn send(&self, value: serde_json::Value) {
        self.writer
            .send(serde_json::to_string(&value).expect("serialize"))
            .expect("send command");
    }

    fn send_raw(&self, line: &str) {
        self.writer.send(line.to_owned()).expect("send raw line");
    }

    async fn next_line(&mut self) -> serde_json::Value {
        let line = tokio::time::timeout(std::time::Duration::from_secs(10), self.reader.recv())
            .await
            .expect("rpc output line within timeout")
            .expect("writer task alive");
        serde_json::from_str(&line).expect("valid json line")
    }

    /// Read lines until one matches `predicate`; returns it.
    async fn wait_for(
        &mut self,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        for _ in 0..500 {
            let value = self.next_line().await;
            if predicate(&value) {
                return value;
            }
        }
        panic!("expected protocol line never arrived");
    }
}

/// Spawn a server over an in-memory duplex pair; returns (server task,
/// client, temp session dir).
async fn rpc_pair(label: &str) -> (tokio::task::JoinHandle<pir::Result<()>>, RpcClient, TempDir) {
    rpc_pair_with(label, rpc_factory()).await
}

async fn rpc_pair_with(
    label: &str,
    factory: SessionFactory,
) -> (tokio::task::JoinHandle<pir::Result<()>>, RpcClient, TempDir) {
    let temp = TempDir::new(label);
    let server = RpcServer::new(temp.0.clone(), factory);
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let handle = tokio::spawn(async move { server.serve(server_read, server_write).await });

    let (client_read, mut client_write) = tokio::io::split(client_stream);

    // Reader: forward server output lines to the client receiver.
    let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(client_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    // Writer: pull lines from the client sender and push them to the server.
    let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(mut line) = write_rx.recv().await {
            line.push('\n');
            if client_write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = client_write.flush().await;
        }
    });

    (
        handle,
        RpcClient {
            writer: write_tx,
            reader: line_rx,
        },
        temp,
    )
}

#[tokio::test]
async fn rpc_hello_unknown_create_and_list() {
    let (_server, mut client, _temp) = rpc_pair("basics").await;

    client.send(serde_json::json!({ "id": "h1", "type": "hello" }));
    let hello = client
        .wait_for(|value| {
            value["type"] == "response" && value["command"] == "hello" && value["id"] == "h1"
        })
        .await;
    assert_eq!(hello["success"], serde_json::json!(true));
    assert_eq!(
        hello["data"]["version"],
        serde_json::json!(pir::protocol::PROTOCOL_VERSION)
    );

    client.send(serde_json::json!({ "type": "bogus-command" }));
    let unknown = client
        .wait_for(|value| value["command"] == "bogus-command")
        .await;
    assert_eq!(unknown["success"], serde_json::json!(false));
    assert!(unknown["error"]
        .as_str()
        .expect("error string")
        .contains("Unknown command"));

    client.send(serde_json::json!({ "type": "create" }));
    let created = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;
    let session_id = created["data"]["id"]
        .as_str()
        .expect("create returns an id")
        .to_owned();

    // The lazy file-creation rule: a fresh session has no file (and is not
    // listed) until its first user message.
    client.send(serde_json::json!({ "type": "list" }));
    let empty_list = client
        .wait_for(|value| value["command"] == "list" && value["success"] == true)
        .await;
    assert_eq!(
        empty_list["data"]["sessions"].as_array().map(Vec::len),
        Some(0),
        "no session file exists before the first conversation message"
    );

    client.send(serde_json::json!({ "id": "p0", "type": "prompt", "text": "materialize" }));
    let _prompt_ack = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p0")
        .await;
    client.wait_for(|value| value["type"] == "agent_end").await;

    client.send(serde_json::json!({ "type": "list" }));
    let list = client
        .wait_for(|value| value["command"] == "list" && value["success"] == true)
        .await;
    let ids: Vec<&str> = list["data"]["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .filter_map(|session| session["id"].as_str())
        .collect();
    assert!(
        ids.contains(&session_id.as_str()),
        "created session missing from list: {ids:?}"
    );

    // Parse failures answer as `command: "parse"` responses.
    client.send_raw("this is not json");
    let parse = client.wait_for(|value| value["command"] == "parse").await;
    assert_eq!(parse["success"], serde_json::json!(false));
}

#[tokio::test]
async fn rpc_prompt_streams_events_and_reports_state() {
    let (_server, mut client, _temp) = rpc_pair("prompt-events").await;

    // attach with no argument opens (or creates) the most recent session.
    client.send(serde_json::json!({ "type": "attach" }));
    let attached = client.wait_for(|value| value["command"] == "attach").await;
    assert_eq!(attached["success"], serde_json::json!(true));
    assert_eq!(attached["data"]["model"], serde_json::json!("fake-model"));

    client.send(serde_json::json!({ "id": "p1", "type": "prompt", "text": "hi" }));
    let accepted = client
        .wait_for(|value| {
            value["type"] == "response" && value["command"] == "prompt" && value["id"] == "p1"
        })
        .await;
    assert_eq!(accepted["success"], serde_json::json!(true));

    // Events stream while the run executes; the run terminates at agent_end.
    let agent_end = client.wait_for(|value| value["type"] == "agent_end").await;
    assert_eq!(agent_end["turns"], serde_json::json!(1));
    let terminal_text = agent_end["terminal"]["content"][0]["text"]
        .as_str()
        .expect("terminal text");
    assert_eq!(terminal_text, "rpc reply");

    // State after the run (upstream get_state shape: no `turns`; the run's
    // turn count is on the bare `agent_end` event above).
    client.send(serde_json::json!({ "type": "get_state" }));
    let state = client
        .wait_for(|value| value["command"] == "get_state")
        .await;
    assert_eq!(state["success"], serde_json::json!(true));
    assert_eq!(state["data"]["messageCount"], serde_json::json!(2));
    assert_eq!(
        state["data"]["model"]["id"],
        serde_json::json!("fake-model")
    );

    // Prompting without a message is an error response.
    client.send(serde_json::json!({ "type": "prompt" }));
    let rejected = client
        .wait_for(|value| value["command"] == "prompt" && value["success"] == false)
        .await;
    assert!(rejected["error"].as_str().is_some());
}

#[tokio::test]
async fn rpc_steer_reaches_attached_session_queue() {
    let (_server, mut client, _temp) = rpc_pair("steer").await;

    client.send(serde_json::json!({ "type": "create" }));
    let _created = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;

    client.send(serde_json::json!({ "type": "steer", "text": "do this instead" }));
    let steered = client.wait_for(|value| value["command"] == "steer").await;
    assert_eq!(steered["success"], serde_json::json!(true));

    client.send(serde_json::json!({ "type": "abort" }));
    let aborted = client.wait_for(|value| value["command"] == "abort").await;
    assert_eq!(aborted["success"], serde_json::json!(true));
}

// ---------------------------------------------------------------------------
// Regression: multi-turn runs persist every produced message (P0-1)
// ---------------------------------------------------------------------------

struct EchoTool;

impl AgentTool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echoes"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("tool-result") })
    }
}

/// A tool producing a large result (drives the keep-recent cut budget in the
/// overflow-recovery test).
struct BigResultTool;

impl AgentTool for BigResultTool {
    fn name(&self) -> &'static str {
        "big"
    }
    fn description(&self) -> &'static str {
        "returns a large result"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("x".repeat(4000)) })
    }
}

/// A 2-turn run (assistant(toolCall) -> toolResult -> assistant) must persist
/// the intermediate assistant message too: upstream persists every
/// `message_end` during the run (agent-session.ts:1144-1163). Persisting only
/// the terminal assistant message orphaned the tool result, which broke the
/// next provider request, compaction file-op extraction, and
/// firstKeptEntryId mapping.
#[tokio::test]
async fn multi_turn_run_persists_intermediate_assistant_messages() {
    let temp = TempDir::new("multi-turn-persist");
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let path = session.path().to_path_buf();
    let registry = fake_registry(Arc::new(|index, context| match index {
        0 => assistant(vec![tool_call("call-1", "echo")], StopReason::ToolUse),
        // The next request must carry the persisted intermediate assistant
        // (toolCall) message and its tool result.
        1 => {
            assert!(
                context.messages.iter().any(|message| matches!(message,
                    Message::ToolResult(result) if result.tool_call_id == "call-1"
                )),
                "tool result missing from the follow-up request: {:?}",
                context.messages
            );
            assert!(
                context.messages.iter().any(|message| matches!(message,
                    Message::Assistant(assistant)
                        if assistant.content.iter().any(|block| matches!(block,
                            ContentBlock::ToolCall(call) if call.id == "call-1"
                        ))
                )),
                "intermediate assistant(toolCall) message missing from the follow-up request"
            );
            assistant(vec![text("done")], StopReason::Stop)
        }
        _ => assistant(vec![text("unexpected")], StopReason::Stop),
    }));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    agent
        .prompt("hi", event_handler(EventLog::default()))
        .await
        .expect("prompt");

    // Reopen the session and verify the transcript stored the complete
    // 2-turn sequence: user -> declaration (declareToolChanges) ->
    // assistant(toolCall) -> toolResult -> assistant.
    let reopened = SessionManager::open(&path).expect("reopen session");
    let entries = reopened.build_context_entries().expect("context entries");
    let shapes: Vec<&str> = entries
        .iter()
        .map(|message| match message {
            Message::User(_) => "user",
            Message::Assistant(assistant) => {
                if assistant
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolCall(_)))
                {
                    "assistant(toolCall)"
                } else {
                    "assistant"
                }
            }
            Message::ToolResult(_) => "toolResult",
            Message::System(_) | Message::Developer(_) => "system",
        })
        .collect();
    assert_eq!(
        shapes,
        vec![
            "user",
            "system",
            "assistant(toolCall)",
            "toolResult",
            "assistant"
        ]
    );
}

// ---------------------------------------------------------------------------
// Regression: overflow recovery compacts once and retries (P1-5)
// ---------------------------------------------------------------------------

/// A context-overflow error terminal triggers exactly one compact-and-retry:
/// the failed attempt is omitted from the transcript, the summarizer runs
/// once, and the run is retried once (upstream agent-session.ts:3003-3044).
#[tokio::test]
async fn overflow_error_triggers_one_compact_and_retry() {
    let temp = TempDir::new("overflow-recovery");
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_responder = calls.clone();
    let registry = fake_registry(Arc::new(move |index, context| {
        calls_for_responder.fetch_add(1, Ordering::SeqCst);
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            // The overflow-recovery summarizer call.
            return assistant(
                vec![text("checkpoint summary of the earlier conversation")],
                StopReason::Stop,
            );
        }
        match index {
            // Prompt 1, turn 1: issue the big tool call.
            0 => assistant(vec![tool_call("call-1", "big")], StopReason::ToolUse),
            // Prompt 1, turn 2: done.
            1 => assistant(vec![text("ready")], StopReason::Stop),
            // Prompt 2, turn 1: a context-overflow error terminal.
            2 => {
                let mut message = assistant(Vec::new(), StopReason::Error);
                message.error_message =
                    Some("prompt is too long: 200000 tokens > 16384 maximum".into());
                message
            }
            // Prompt 2 retry after compaction: the failed attempt was omitted
            // and the compacted summary heads the context.
            _ => {
                assert!(
                    context.messages.iter().any(|message| matches!(message,
                        Message::User(user)
                            if user.content.iter().any(|block| matches!(block,
                                ContentBlock::Text(text)
                                    if text.text.starts_with(pir::compaction::prompts::COMPACTION_SUMMARY_PREFIX)
                            ))
                    )),
                    "compaction summary missing from the retry request: {:?}",
                    context.messages
                );
                assert!(
                    !context.messages.iter().any(|message| matches!(message,
                        Message::Assistant(assistant) if assistant.stop_reason == StopReason::Error
                    )),
                    "failed attempt leaked into the retry request: {:?}",
                    context.messages
                );
                assistant(vec![text("recovered after compaction")], StopReason::Stop)
            }
        }
    }));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BigResultTool));
    // A large context window keeps the threshold compaction hook idle; the
    // keep-recent budget drives the recovery cut instead.
    let mut model = fake_model();
    model.context_window = 1_000_000;
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model,
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: None,
        compaction: Some(pir::compaction::CompactionConfig {
            keep_recent_tokens: 500,
            ..pir::compaction::CompactionConfig::default()
        }),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    // Prompt 1 fills the transcript with a big tool result.
    agent
        .prompt("setup", event_handler(EventLog::default()))
        .await
        .expect("setup prompt");

    // Prompt 2 errors with a context overflow and recovers.
    let terminal = agent
        .prompt("hi", event_handler(EventLog::default()))
        .await
        .expect("prompt recovers");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("recovered after compaction")
    );
    let transcript = agent.transcript().await;
    assert!(
        transcript.iter().any(|message| matches!(
            message,
            Message::User(user)
                if user.content.iter().any(|block| matches!(
                    block,
                    ContentBlock::Text(text) if text.text == "hi"
                ))
        )),
        "the ephemeral overflow recovery lost the user prompt: {transcript:?}"
    );

    // Exactly one compact-and-retry: prompt 1 (2 calls) + failed main +
    // summarizer + retry = 5 stream calls in total.
    assert_eq!(calls.load(Ordering::SeqCst), 5);
}

/// Session-backed overflow recovery must be DURABLE (upstream
/// `_runAutoCompaction("overflow")`): the recovery compaction is persisted as
/// a session entry, the retry request is built from the projected (compacted)
/// history, and the NEXT prompt rebuilds from the compacted projection
/// instead of re-overflowing on the full uncompacted transcript. Exactly one
/// compaction entry lands in storage and the failed attempt never persists.
#[tokio::test]
async fn session_overflow_recovery_persists_compaction_entry() {
    let temp = TempDir::new("overflow-session-recovery");
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let session_path = session.path().to_path_buf();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_responder = calls.clone();
    let registry = fake_registry(Arc::new(move |index, context| {
        calls_for_responder.fetch_add(1, Ordering::SeqCst);
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            // The overflow-recovery summarizer call. It must run exactly once
            // across the whole test (one recovery attempt per prompt, and the
            // follow-up prompt must not re-compact).
            return assistant(
                vec![text("checkpoint summary of the earlier conversation")],
                StopReason::Stop,
            );
        }
        match index {
            // Prompt 1, turn 1: issue the big tool call.
            0 => assistant(vec![tool_call("call-1", "big")], StopReason::ToolUse),
            // Prompt 1, turn 2: done.
            1 => assistant(vec![text("ready")], StopReason::Stop),
            // Prompt 2, turn 1: a context-overflow error terminal.
            2 => {
                let mut message = assistant(Vec::new(), StopReason::Error);
                message.error_message =
                    Some("prompt is too long: 200000 tokens > 16384 maximum".into());
                message
            }
            // Prompt 2 retry (index 4) and prompt 3 (index 5): both are built
            // FROM the session projection — the compaction summary heads the
            // context and the failed attempt never appears.
            _ => {
                assert!(
                    context.messages.iter().any(|message| matches!(message,
                        Message::User(user)
                            if user.content.iter().any(|block| matches!(block,
                                ContentBlock::Text(text)
                                    if text.text.starts_with(pir::compaction::prompts::COMPACTION_SUMMARY_PREFIX)
                            ))
                    )),
                    "compaction summary missing from the projected request: {:?}",
                    context.messages
                );
                assert!(
                    !context.messages.iter().any(|message| matches!(message,
                        Message::Assistant(assistant) if assistant.stop_reason == StopReason::Error
                    )),
                    "failed attempt leaked into the projected request: {:?}",
                    context.messages
                );
                assistant(vec![text("steady")], StopReason::Stop)
            }
        }
    }));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BigResultTool));
    // A large context window keeps the threshold compaction hook idle; the
    // keep-recent budget drives the recovery cut instead.
    let mut model = fake_model();
    model.context_window = 1_000_000;
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model,
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: Some(session),
        compaction: Some(pir::compaction::CompactionConfig {
            keep_recent_tokens: 500,
            ..pir::compaction::CompactionConfig::default()
        }),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    // Prompt 1 fills the transcript with a big tool result.
    agent
        .prompt("setup", event_handler(EventLog::default()))
        .await
        .expect("setup prompt");

    // Prompt 2 errors with a context overflow and recovers durably.
    let terminal = agent
        .prompt("hi", event_handler(EventLog::default()))
        .await
        .expect("prompt recovers");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("steady")
    );

    // Prompt 3 rebuilds from the compacted projection and does NOT
    // re-compact (no second summarizer call).
    let terminal = agent
        .prompt("again", event_handler(EventLog::default()))
        .await
        .expect("follow-up prompt");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("steady")
    );

    // Prompt 1 (2 calls) + failed main + summarizer + retry + prompt 3 = 6.
    assert_eq!(calls.load(Ordering::SeqCst), 6);

    // Exactly one compaction entry in storage. The raw transcript now KEEPS
    // the failed attempt (upstream persists every `message_end`); the durable
    // omission is a `replacement: null` context edit, so the model-visible
    // projection excludes it while the stored pair stays consistent.
    let reopened = SessionManager::open(&session_path).expect("reopen session");
    let branch = reopened.branch_entries(None).expect("branch entries");
    let compactions = branch
        .iter()
        .filter(|entry| matches!(entry, pir::session_manager::FileEntry::Compaction(_)))
        .count();
    assert_eq!(compactions, 1, "expected exactly one compaction entry");
    let error_entry_ids: Vec<String> = branch
        .iter()
        .filter_map(|entry| match entry {
            pir::session_manager::FileEntry::Message(message) => match &message.message {
                pir::session_manager::types::StoredMessage::Assistant(assistant)
                    if assistant.stop_reason == StopReason::Error =>
                {
                    Some(message.id.clone())
                }
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        error_entry_ids.len(),
        1,
        "the failed attempt's raw entry must be retained (persisted as a pair with any results)"
    );
    let omitted = omitted_entry_ids(&branch);
    assert!(
        omitted.contains(&error_entry_ids[0]),
        "the failed attempt entry {error_entry_ids:?} is not omitted by a context edit: {omitted:?}"
    );
    let projected = reopened.build_context_entries().expect("context entries");
    assert!(
        !projected.iter().any(|message| matches!(
            message,
            Message::Assistant(assistant) if assistant.stop_reason == StopReason::Error
        )),
        "failed attempt leaked into the model-visible projection: {projected:?}"
    );
}

// ---------------------------------------------------------------------------
// Regression: length stop + tool calls persists a PAIRED attempt (#11227)
// ---------------------------------------------------------------------------

/// A `length` terminal with tool calls persists the assistant AND its synthetic
/// "was not executed" results, then the post-run recovery omits both sides with
/// `replacement: null` context edits (upstream `_omitRecoveryAttempt`).
///
/// Regression for upstream #11227: the old sink skipped the failed assistant
/// but persisted its derived tool results, leaving orphan `toolResult` entries
/// in the durable transcript. On resume every provider request was rejected.
/// The test covers parallel calls and an empty `toolCallId`, then reopens the
/// session and drives a full prompt to prove the first request's shape is
/// legal.
#[tokio::test]
async fn length_with_tool_calls_omits_attempt_and_results_together() {
    let temp = TempDir::new("length-omission");
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let session_path = session.path().to_path_buf();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_responder = calls.clone();
    let registry = fake_registry(Arc::new(move |index, context| {
        calls_for_responder.fetch_add(1, Ordering::SeqCst);
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("checkpoint summary")], StopReason::Stop);
        }
        match index {
            // Prompt 1: enough history for the recovery compaction to have a
            // cut point.
            0 => assistant(vec![tool_call("setup-big", "big")], StopReason::ToolUse),
            1 => assistant(vec![text("ready")], StopReason::Stop),
            // Prompt 2: the truncated turn. Three parallel calls, one with an
            // empty id: an empty id must match only itself, never wildcard.
            2 => assistant(
                vec![
                    text("partial"),
                    tool_call("call-a", "echo"),
                    tool_call("call-b", "echo"),
                    tool_call("", "echo"),
                ],
                StopReason::Length,
            ),
            // Retry after the durable recovery compaction (index 3 is the
            // summarizer, short-circuited above).
            _ => {
                assert_tool_pairs(&context.messages);
                assert!(
                    !context.messages.iter().any(|message| matches!(
                        message,
                        Message::Assistant(assistant) if assistant.stop_reason == StopReason::Length
                    )),
                    "omitted length assistant leaked into the retry request: {:?}",
                    context.messages
                );
                for id in ["call-a", "call-b", ""] {
                    assert!(
                        !context.messages.iter().any(|message| matches!(
                            message,
                            Message::ToolResult(result) if result.tool_call_id == id
                        )),
                        "omitted synthetic result {id:?} leaked into the retry request: {:?}",
                        context.messages
                    );
                }
                assistant(vec![text("recovered")], StopReason::Stop)
            }
        }
    }));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BigResultTool));
    tools.register(Arc::new(EchoTool));
    let mut model = fake_model();
    model.context_window = 1_000_000;
    let compaction = pir::compaction::CompactionConfig {
        keep_recent_tokens: 500,
        ..pir::compaction::CompactionConfig::default()
    };
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: model.clone(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: Some(session),
        compaction: Some(compaction.clone()),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));
    // A length turn with tool calls normally continues (the synthetic results
    // invite a re-issue); end it explicitly so the run's transitive tail is
    // assistant(length) -> three results, the exact #11227 shape.
    agent.set_finish_turn(Some(Arc::new(|turn: FinishTurnContext| {
        Box::pin(async move {
            (turn.message.stop_reason == StopReason::Length).then_some(FinishTurnAction::End)
        })
    })));

    agent
        .prompt("setup", event_handler(EventLog::default()))
        .await
        .expect("setup prompt");
    let terminal = agent
        .prompt("hi", event_handler(EventLog::default()))
        .await
        .expect("length prompt recovers");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("recovered")
    );

    // The raw transcript keeps the complete pair: one length assistant and its
    // three synthetic results.
    let reopened = SessionManager::open(&session_path).expect("reopen session");
    let branch = reopened.branch_entries(None).expect("branch entries");
    let attempt_entry_ids = stored_attempt_entry_ids(&branch);
    assert_eq!(
        attempt_entry_ids.len(),
        4,
        "expected the length assistant and its three synthetic results in raw storage: {branch:?}"
    );
    let omitted = omitted_entry_ids(&branch);
    for entry_id in &attempt_entry_ids {
        assert!(
            omitted.contains(entry_id),
            "attempt entry {entry_id} was not omitted (context edits: {omitted:?})"
        );
    }
    // The model-visible projection has neither side, so the pair can never be
    // split into an orphan result.
    let projected = reopened.build_context_entries().expect("context entries");
    assert_tool_pairs(&projected);
    assert!(
        !projected.iter().any(|message| matches!(
            message,
            Message::Assistant(assistant) if assistant.stop_reason == StopReason::Length
        )),
        "length assistant leaked into the projection: {projected:?}"
    );
    for id in ["call-a", "call-b", ""] {
        assert!(
            !projected.iter().any(|message| matches!(
                message,
                Message::ToolResult(result) if result.tool_call_id == id
            )),
            "synthetic result {id:?} leaked into the projection: {projected:?}"
        );
    }

    // Resume the stored session and drive a real prompt: the first provider
    // request after resume must be a legal assistant/toolResult sequence
    // (the #11227 symptom was every resumed request being rejected).
    let resumed_session = SessionManager::open(&session_path).expect("reopen session for resume");
    let resumed_registry = fake_registry(Arc::new(|_index, context| {
        assert_tool_pairs(&context.messages);
        assert!(
            !context.messages.iter().any(|message| matches!(
                message,
                Message::Assistant(assistant) if assistant.stop_reason == StopReason::Length
            )),
            "omitted attempt leaked into the resumed request: {:?}",
            context.messages
        );
        assistant(vec![text("resumed")], StopReason::Stop)
    }));
    let resumed = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: resumed_registry,
        model,
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(resumed_session),
        compaction: Some(compaction),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));
    let terminal = resumed
        .prompt("resume", event_handler(EventLog::default()))
        .await
        .expect("resumed prompt");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("resumed")
    );
}

/// A non-overflow error terminal is an ordinary failed turn: it stays in the
/// durable transcript and in the model-visible projection (no recovery
/// omission). Only overflow / recoverable-length terminals take the omission
/// path.
#[tokio::test]
async fn non_overflow_error_is_not_omitted() {
    let temp = TempDir::new("plain-error-persist");
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let session_path = session.path().to_path_buf();
    let registry = fake_registry(Arc::new(|_index, _context| {
        let mut message = assistant(vec![text("partial")], StopReason::Error);
        message.error_message = Some("connection reset by peer".into());
        message
    }));
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        // The recovery check is active; the error classification must keep it
        // from firing.
        compaction: Some(pir::compaction::CompactionConfig::default()),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));
    let terminal = agent
        .prompt("hi", event_handler(EventLog::default()))
        .await
        .expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Error);

    let reopened = SessionManager::open(&session_path).expect("reopen session");
    let branch = reopened.branch_entries(None).expect("branch entries");
    assert!(
        omitted_entry_ids(&branch).is_empty(),
        "a non-overflow error must not produce context-edit omissions"
    );
    let projected = reopened.build_context_entries().expect("context entries");
    assert!(
        projected.iter().any(|message| matches!(
            message,
            Message::Assistant(assistant) if assistant.stop_reason == StopReason::Error
        )),
        "non-overflow error must stay model-visible: {projected:?}"
    );
}

// ---------------------------------------------------------------------------
// Regression: steering survives a failed prompt (P2-14)
// ---------------------------------------------------------------------------

/// A prompt that fails mid-run (session persistence error) must not disable
/// steering: the run-owned steering inbox is restored on every exit path, so
/// a message queued across the failed run is delivered by the next prompt.
#[tokio::test]
async fn failed_prompt_keeps_steering_deliverable() {
    let temp = TempDir::new("inbox-guard");
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    let session_path = session.path().to_path_buf();
    // Flush the session file once so later appends target an existing file.
    session
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text("warmup")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("warmup append");
    let registry = fake_registry(Arc::new(|_index, context| {
        // The successful retry observes the steering redirect.
        assert!(
            user_texts(context).iter().any(|text| text == "redirect"),
            "steering message missing from the recovered prompt: {:?}",
            user_texts(context)
        );
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    agent.steer("redirect");
    // Break persistence: the in-flight run fails on the user-message append
    // AFTER taking ownership of the steering inbox.
    std::fs::remove_file(&session_path).expect("remove session file");
    let failed = agent
        .prompt("first", event_handler(EventLog::default()))
        .await;
    assert!(failed.is_err(), "expected the broken session to fail");

    // Repair persistence; the queued steering must still be deliverable.
    std::fs::write(&session_path, "").expect("recreate session file");
    let terminal = agent
        .prompt("second", event_handler(EventLog::default()))
        .await
        .expect("prompt after failure");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("ok")
    );
}

// ---------------------------------------------------------------------------
// P2-4 (updated by the gap-closure wave): a prompt while streaming is not
// refused — it is synchronously acknowledged as `queued` (upstream rpc-mode
// queue-poll discipline) and never runs concurrently with the first run.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpc_rapid_prompts_second_is_queued_never_concurrent() {
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let (_server, mut client, _temp) =
        rpc_pair_with("rpc-race", rpc_factory_gated(gate_rx, Arc::clone(&entered))).await;

    client.send(serde_json::json!({ "type": "create" }));
    let _created = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;

    // First prompt: accepted, its run parks inside the gate tool.
    client.send(serde_json::json!({ "id": "p1", "type": "prompt", "text": "start" }));
    let first = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p1")
        .await;
    assert_eq!(first["success"], serde_json::json!(true), "{first}");
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        entered.load(Ordering::SeqCst) > 0,
        "gate tool never started"
    );

    // Second prompt while the first run is in flight, with no
    // `streamingBehavior`: upstream agent-session.ts:2012-2022 REJECTS it
    // ("Agent is already processing. …"); it is never silently queued.
    client.send(serde_json::json!({ "id": "p2", "type": "prompt", "text": "second" }));
    let second = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p2")
        .await;
    assert_eq!(second["success"], serde_json::json!(false), "{second}");
    assert!(
        second["error"]
            .as_str()
            .is_some_and(|error| error.contains("Agent is already processing")),
        "{second}"
    );

    // …but with `streamingBehavior: "followUp"` the same prompt queues
    // without ever running concurrently with the first run.
    client.send(serde_json::json!({
        "id": "p2q",
        "type": "prompt",
        "text": "second",
        "streamingBehavior": "followUp"
    }));
    let second_queued = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p2q")
        .await;
    assert_eq!(
        second_queued["success"],
        serde_json::json!(true),
        "{second_queued}"
    );
    assert_eq!(
        second_queued["data"]["disposition"],
        serde_json::json!("queued"),
        "{second_queued}"
    );

    // Release the gate; the first run settles and the claim is released, so
    // the next prompt is accepted again.
    let _ = gate_tx.send(true);
    // The first run ends (the queued follow-up is folded into that run's
    // settle). Poll the protocol state until the session is idle so p3 cannot
    // race the claim release.
    client.wait_for(|value| value["type"] == "agent_end").await;
    for _ in 0..100 {
        client.send(serde_json::json!({ "type": "get_state" }));
        let state = client
            .wait_for(|value| value["command"] == "get_state")
            .await;
        if state["data"]["isStreaming"] == serde_json::json!(false) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    client.send(serde_json::json!({ "id": "p3", "type": "prompt", "text": "third" }));
    let third = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p3")
        .await;
    assert_eq!(third["success"], serde_json::json!(true), "{third}");
}

// ---------------------------------------------------------------------------
// P2-5: prompt/steer/follow_up responses carry data.disposition and the
// follow_up command exists.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpc_prompt_steer_and_follow_up_report_dispositions() {
    let (_server, mut client, _temp) = rpc_pair("dispositions").await;

    client.send(serde_json::json!({ "type": "create" }));
    let _created = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;

    // An accepted prompt reports disposition "started" (upstream prompt()
    // preflight, agent-session.ts:2109).
    client.send(serde_json::json!({ "id": "p1", "type": "prompt", "text": "hi" }));
    let accepted = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p1")
        .await;
    assert_eq!(accepted["success"], serde_json::json!(true));
    assert_eq!(accepted["data"]["disposition"], "started", "{accepted}");
    // `agent_end` precedes the idle transition; wait for the settle edge so
    // the steer below is handled as idle input rather than queued.
    client
        .wait_for(|value| value["type"] == "agent_settled")
        .await;
    // The event is forwarded from the session's settle edge; give the idle
    // flag propagation one scheduler turn on a loaded runner before sending
    // the idle steer.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // steer while idle is "handled" (delivered immediately at the next poll);
    // while a run streams it reports "queued" (pinned upstream contract).
    client.send(serde_json::json!({ "type": "steer", "text": "redirect" }));
    let steered = client.wait_for(|value| value["command"] == "steer").await;
    assert_eq!(steered["success"], serde_json::json!(true), "{steered}");
    assert_eq!(steered["data"]["disposition"], "handled", "{steered}");

    // follow_up is a passthrough command with the same shape.
    client.send(serde_json::json!({ "type": "follow_up", "text": "afterwards" }));
    let followed = client
        .wait_for(|value| value["command"] == "follow_up")
        .await;
    assert_eq!(followed["success"], serde_json::json!(true), "{followed}");
    assert_eq!(followed["data"]["disposition"], "handled", "{followed}");

    // The queued follow-up reaches the session's pending queue.
    let state = {
        client.send(serde_json::json!({ "type": "get-state" }));
        client
            .wait_for(|value| value["command"] == "get-state")
            .await
    };
    assert_eq!(state["success"], serde_json::json!(true), "{state}");
}

// ---------------------------------------------------------------------------
// P2-8: a writer that dies mid-run ends the session (bounded stdout channel).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpc_writer_death_during_a_run_ends_the_session() {
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let temp = TempDir::new("rpc-midrun-death");
    let server = RpcServer::new(
        temp.0.clone(),
        rpc_factory_gated(gate_rx, Arc::clone(&entered)),
    );

    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let handle = tokio::spawn(async move { server.serve(server_read, server_write).await });

    let (client_read, mut client_write) = tokio::io::split(client_stream);
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // The reader task owns the client read half; dropping it (on the kill
    // signal) is what breaks the server's writes.
    let (kill_tx, mut kill_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let mut lines = BufReader::new(client_read).lines();
        loop {
            tokio::select! {
                _ = kill_rx.changed() => break,
                read = lines.next_line() => match read {
                    Ok(Some(line)) => {
                        if line_tx.send(line).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        }
        drop(lines);
    });

    async fn next_response(
        line_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    ) -> serde_json::Value {
        let line = tokio::time::timeout(std::time::Duration::from_secs(10), line_rx.recv())
            .await
            .expect("rpc line within timeout")
            .expect("writer alive");
        serde_json::from_str(&line).expect("valid json line")
    }

    client_write
        .write_all(b"{\"type\":\"create\"}\n")
        .await
        .expect("write create");
    client_write.flush().await.expect("flush");
    let created = next_response(&mut line_rx).await;
    assert_eq!(created["command"], "create", "{created}");

    // Start a run parked in the gate tool, then kill the output side.
    client_write
        .write_all(b"{\"id\":\"p1\",\"type\":\"prompt\",\"text\":\"start\"}\n")
        .await
        .expect("write prompt");
    client_write.flush().await.expect("flush");
    let ack = next_response(&mut line_rx).await;
    assert_eq!(ack["success"], serde_json::json!(true), "{ack}");
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // Kill the client side entirely (both halves): the reader task drops its
    // half on the kill signal, the test drops the write half. The server's
    // writer task then fails on its next write, and the serve loop must end
    // instead of streaming into a closed pipe forever.
    drop(line_rx);
    let _ = kill_tx.send(true);
    drop(client_write);
    let _ = gate_tx.send(true);
    let served = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
        .await
        .expect("serve must end after the writer died mid-run");
    let result = served.expect("serve task joins");
    assert!(
        result.is_err(),
        "serve must report the dead writer as an error, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Pinned contracts #2/#3: disposition wire values, real steer/follow_up
// dispositions, and the queued-message getters.
// ---------------------------------------------------------------------------

#[test]
fn disposition_wire_values_match_upstream() {
    assert_eq!(
        serde_json::to_value(pir::agent_session::Disposition::Started).unwrap(),
        serde_json::json!("started")
    );
    assert_eq!(
        serde_json::to_value(pir::agent_session::Disposition::Handled).unwrap(),
        serde_json::json!("handled")
    );
    assert_eq!(
        serde_json::to_value(pir::agent_session::Disposition::Queued).unwrap(),
        serde_json::json!("queued")
    );
}

#[tokio::test]
async fn steer_and_follow_up_report_real_dispositions_and_queue_texts() {
    let temp = TempDir::new("dispositions-idle");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    // Idle: the messages will be delivered immediately (handled).
    assert_eq!(
        agent.input_disposition(),
        pir::agent_session::Disposition::Started
    );
    assert_eq!(agent.steer("s1"), pir::agent_session::Disposition::Handled);
    assert_eq!(
        agent.follow_up("f1"),
        pir::agent_session::Disposition::Handled
    );
    // The queued texts are visible without draining.
    assert_eq!(agent.get_steering_messages().await, vec!["s1".to_owned()]);
    assert_eq!(agent.get_follow_up_messages().await, vec!["f1".to_owned()]);
    assert_eq!(agent.pending_message_count(), 2);
}

#[tokio::test]
async fn steer_while_streaming_reports_queued() {
    let temp = TempDir::new("dispositions-streaming");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(Some((gate_rx, entered.clone()))),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("start", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_tool_entry(&entered).await;
    assert_eq!(
        agent.steer("mid-run"),
        pir::agent_session::Disposition::Queued
    );
    assert_eq!(
        agent.get_steering_messages().await,
        vec!["mid-run".to_owned()]
    );
    let _ = gate_tx.send(true);
    prompt_task
        .await
        .expect("prompt task")
        .expect("prompt result");
}

// ---------------------------------------------------------------------------
// Per-message persistence (upstream agent-session.ts:1144-1163): messages are
// in session storage DURING the run, before it settles.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn assistant_message_is_persisted_before_the_run_settles() {
    let temp = TempDir::new("midrun-persistence");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let session_path_for_check = Arc::new(Mutex::new(None::<std::path::PathBuf>));
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    *session_path_for_check.lock().unwrap() = Some(session.path().to_path_buf());
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(Some((gate_rx, entered.clone()))),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("start", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_tool_entry(&entered).await;

    // Mid-run: the tool is still executing, its result does not exist yet —
    // but the first assistant message is already in storage.
    let path = session_path_for_check
        .lock()
        .unwrap()
        .clone()
        .expect("session path");
    let mid_run = std::fs::read_to_string(&path).expect("session file readable mid-run");
    assert!(
        mid_run.contains("\"role\":\"assistant\""),
        "assistant message must be persisted during the run: {mid_run}"
    );

    let _ = gate_tx.send(true);
    prompt_task
        .await
        .expect("prompt task")
        .expect("prompt result");
}

// ---------------------------------------------------------------------------
// recordBashResult (upstream agent-session.ts:2293-2395)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn record_bash_result_appends_context_and_excluded_records() {
    let temp = TempDir::new("bash-record");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let path = session.path().to_path_buf();
    // The on-disk file is created lazily on the first user-or-assistant
    // message (upstream rule); seed one so the disk-shape assertions below
    // have a file to read.
    let mut session = session;
    session
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text("seed")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("user append");
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let result = pir::agent_session::BashResult {
        output: "listed".into(),
        exit_code: Some(0),
        cancelled: false,
        truncated: false,
        full_output_path: None,
    };
    agent.record_bash_result("ls", &result, false).await;
    let excluded = pir::agent_session::BashResult {
        output: "secret".into(),
        exit_code: Some(0),
        cancelled: false,
        truncated: false,
        full_output_path: None,
    };
    agent
        .record_bash_result("secret-cmd", &excluded, true)
        .await;

    let stored = std::fs::read_to_string(&path).expect("session file");
    // Both records are upstream `bashExecution` message entries (not custom
    // entries): the in-context one participates in the model context, the
    // `!!` one carries `excludeFromContext` and stays transcript-only.
    let lines: Vec<serde_json::Value> = stored
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let included = lines
        .iter()
        .find(|line| {
            line["type"] == "message"
                && line["message"]["role"] == "bashExecution"
                && line["message"]["command"] == "ls"
        })
        .expect("in-context bash record missing");
    assert_eq!(included["message"]["output"], "listed");
    assert!(
        included["message"].get("excludeFromContext").is_none(),
        "an in-context record must not be marked excluded: {included}"
    );
    let excluded = lines
        .iter()
        .find(|line| {
            line["type"] == "message"
                && line["message"]["role"] == "bashExecution"
                && line["message"]["command"] == "secret-cmd"
        })
        .expect("excluded bash record missing");
    assert_eq!(excluded["message"]["excludeFromContext"], true);
    assert!(stored.contains("secret-cmd"), "details missing: {stored}");
}

/// P1-B: a `!` bash command that outlives a `/new`/`/resume` must not record
/// its `<bash-execution>` into the replacement transcript. The storage
/// generation moves on every swap; a recording made with the old generation
/// is refused, while the current generation still lands in the new session.
#[tokio::test]
async fn bash_records_are_guarded_by_the_session_storage_generation() {
    let temp = TempDir::new("bash-generation");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let seed = |timestamp: u64| {
        Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text("seed")],
            tools_added: None,
            tools_removed: None,
            timestamp,
            sections: None,
        })
    };

    let mut first = SessionManager::create(".", &temp.0).expect("create session");
    first.append_message(seed(1)).expect("user append");
    let first_path = first.path().to_path_buf();
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(first),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));
    let generation = agent.session_storage_generation();

    let result = pir::agent_session::BashResult {
        output: "old".into(),
        exit_code: Some(0),
        cancelled: false,
        truncated: false,
        full_output_path: None,
    };
    assert!(
        agent
            .record_bash_result_at_generation("echo old", &result, false, generation)
            .await,
        "the current generation records normally"
    );

    // The switch: a second session file replaces the first.
    let mut second = SessionManager::create(".", &temp.0).expect("create replacement");
    second.append_message(seed(2)).expect("user append");
    let second_path = second.path().to_path_buf();
    assert!(agent.replace_session_storage(second).await);
    let current = agent.session_storage_generation();
    assert_ne!(
        current, generation,
        "replace_session_storage must move the generation"
    );

    // The old command completing now must be refused…
    assert!(
        !agent
            .record_bash_result_at_generation("echo stale", &result, false, generation)
            .await,
        "a completion from the outgoing transcript must not record"
    );
    // …while work started against the new transcript records.
    assert!(
        agent
            .record_bash_result_at_generation("echo new", &result, false, current)
            .await
    );

    let outgoing = std::fs::read_to_string(&first_path).expect("outgoing session file");
    assert!(outgoing.contains("echo old"), "{outgoing}");
    assert!(
        !outgoing.contains("echo stale"),
        "the stale completion landed in the outgoing session: {outgoing}"
    );
    assert!(
        !outgoing.contains("echo new"),
        "new-transcript work leaked into the outgoing session: {outgoing}"
    );
    let attached = std::fs::read_to_string(&second_path).expect("replacement session file");
    assert!(attached.contains("echo new"), "{attached}");
    assert!(
        !attached.contains("echo old"),
        "the outgoing command was recorded into the new session: {attached}"
    );
    assert!(
        !attached.contains("echo stale"),
        "the stale completion was recorded into the new session: {attached}"
    );
}

// ---------------------------------------------------------------------------
// branchWithSummary exposure (upstream session-manager.ts:1600-1625)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn branch_with_summary_appends_a_branch_summary_entry() {
    let temp = TempDir::new("branch-summary");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    let user_id = session
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text("first")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("user append");
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let entry_id = agent
        .branch_with_summary(Some(&user_id), "abandoned path summary", None, false)
        .await
        .expect("storage configured")
        .expect("branch");
    assert!(!entry_id.is_empty());

    let entries = agent
        .with_session_manager(|manager| manager.entries())
        .await
        .expect("session manager");
    let summary = entries
        .iter()
        .find_map(|entry| match entry {
            pir::session_manager::FileEntry::BranchSummary(inner) => Some(inner),
            _ => None,
        })
        .expect("branch summary entry");
    assert_eq!(summary.summary, "abandoned path summary");
    assert_eq!(summary.from_id, user_id);
    assert_eq!(summary.parent_id.as_deref(), Some(user_id.as_str()));
}

// ---------------------------------------------------------------------------
// Queue regression (single-track command inbox): enqueue order, peek, clear,
// and the one-at-a-time drain discipline.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn queued_commands_keep_fifo_order_and_clear_drains_all() {
    let temp = TempDir::new("queue-fifo");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    // Enqueues land on the single consumer queue in call order; the peek
    // getters and the pending count agree with it.
    assert_eq!(agent.steer("s1"), pir::agent_session::Disposition::Handled);
    assert_eq!(agent.steer("s2"), pir::agent_session::Disposition::Handled);
    assert_eq!(
        agent.follow_up("f1"),
        pir::agent_session::Disposition::Handled
    );
    assert_eq!(agent.steer("s3"), pir::agent_session::Disposition::Handled);
    assert_eq!(
        agent.follow_up("f2"),
        pir::agent_session::Disposition::Handled
    );
    assert_eq!(
        agent.get_steering_messages().await,
        vec!["s1".to_owned(), "s2".to_owned(), "s3".to_owned()]
    );
    assert_eq!(
        agent.get_follow_up_messages().await,
        vec!["f1".to_owned(), "f2".to_owned()]
    );
    assert_eq!(agent.pending_message_count(), 5);

    // Peek semantics: peeking does not drain.
    assert_eq!(agent.get_steering_messages().await.len(), 3);

    // `clear_queue` drains everything in order and resets the count.
    let cleared = agent.clear_queue().await;
    assert_eq!(
        cleared.steering,
        vec!["s1".to_owned(), "s2".to_owned(), "s3".to_owned()]
    );
    assert_eq!(cleared.follow_up, vec!["f1".to_owned(), "f2".to_owned()]);
    assert_eq!(agent.pending_message_count(), 0);
    assert!(agent.get_steering_messages().await.is_empty());
    assert!(agent.get_follow_up_messages().await.is_empty());
}

/// A session switch creates a new runtime with EMPTY queues upstream
/// (`agent-session-runtime.ts` `newSession` -> `createRuntime`). pir's queue
/// facade survives the storage swap, so the swap itself must drain it: a
/// message queued for session A must never be injected into the first prompt
/// of session B (nor linger invisibly after the pending block clears).
#[tokio::test]
async fn session_swap_drops_queued_steering_and_follow_up() {
    let temp = TempDir::new("queue-swap");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(SessionManager::in_memory(".", None).expect("in-memory session")),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    assert_eq!(
        agent.steer("belongs to the old session"),
        pir::agent_session::Disposition::Handled
    );
    assert_eq!(
        agent.follow_up("also old"),
        pir::agent_session::Disposition::Handled
    );
    assert_eq!(agent.pending_message_count(), 2);

    let replacement = SessionManager::in_memory(".", None).expect("replacement session");
    assert!(agent.replace_session_storage(replacement).await);

    assert!(
        agent.get_steering_messages().await.is_empty(),
        "the swap must not carry steering text into the new session"
    );
    assert!(
        agent.get_follow_up_messages().await.is_empty(),
        "the swap must not carry follow-up text into the new session"
    );
    assert_eq!(agent.pending_message_count(), 0);
}

#[tokio::test]
async fn steering_delivers_one_message_per_turn() {
    let temp = TempDir::new("queue-one-at-a-time");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let observed_for_responder = observed.clone();
    let registry = fake_registry(Arc::new(move |index, context| {
        if index == 0 {
            assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
        } else {
            observed_for_responder
                .lock()
                .unwrap()
                .push(user_texts(context));
            assistant(vec![text("after steer")], StopReason::Stop)
        }
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(Some((gate_rx, entered.clone()))),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("start", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_tool_entry(&entered).await;
    agent.steer("s1");
    agent.steer("s2");
    let _ = gate_tx.send(true);
    prompt_task
        .await
        .expect("prompt task")
        .expect("prompt result");

    let seen = observed.lock().unwrap();
    assert!(seen.len() >= 2, "the steered turns never ran: {seen:?}");
    // Default one-at-a-time: the FIRST poll after the tool turn delivers
    // exactly one message, so the first steered request carries s1 only.
    assert!(
        seen[0].iter().any(|text| text == "s1"),
        "s1 missing from the first steered request: {:?}",
        seen[0]
    );
    assert!(
        !seen[0].iter().any(|text| text == "s2"),
        "one-at-a-time violated, s2 arrived with s1: {:?}",
        seen[0]
    );
    // Delivery order is FIFO, and s2 eventually reached the model.
    let delivery_order: Vec<&str> = seen
        .iter()
        .flat_map(|request| request.iter().map(String::as_str))
        .filter(|text| *text == "s1" || *text == "s2")
        .collect();
    assert_eq!(
        delivery_order.first(),
        Some(&"s1"),
        "delivery order: {delivery_order:?}"
    );
    assert!(
        delivery_order.contains(&"s2"),
        "s2 never delivered: {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// Ephemeral-history regression (write-on-replace shared snapshot): every read
// path observes the same transcript after each write.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ephemeral_history_reads_observe_the_same_transcript() {
    let temp = TempDir::new("history-snapshot");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("reply")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    // Empty baseline.
    assert_eq!(agent.message_count().await, 0);
    assert!(agent.transcript().await.is_empty());
    assert!(agent.ephemeral_transcript().await.is_empty());
    assert!(agent.transcript_json().await.is_empty());

    agent
        .prompt("hi", event_handler(EventLog::default()))
        .await
        .expect("prompt");
    agent
        .prompt("again", event_handler(EventLog::default()))
        .await
        .expect("prompt");

    // Every read path agrees on the replaced snapshot.
    assert_eq!(agent.message_count().await, 4);
    let transcript = agent.transcript().await;
    assert_eq!(transcript.len(), 4);
    assert!(matches!(transcript[0], Message::User(_)));
    assert!(matches!(transcript[1], Message::Assistant(_)));
    assert_eq!(text_of_message(&transcript[0]).as_deref(), Some("hi"));
    assert_eq!(text_of_message(&transcript[3]).as_deref(), Some("reply"));
    let ephemeral = agent.ephemeral_transcript().await;
    assert_eq!(ephemeral.len(), transcript.len());
    let json = agent.transcript_json().await;
    assert_eq!(json.len(), 4);
    assert_eq!(json[0]["role"], serde_json::json!("user"));
    assert_eq!(json[1]["role"], serde_json::json!("assistant"));

    // The stats path reads the snapshot without mutating it.
    let stats = agent.session_stats().await;
    assert_eq!(stats.user_messages, 2);
    assert_eq!(stats.assistant_messages, 2);
    assert_eq!(agent.message_count().await, 4);
}

// ---------------------------------------------------------------------------
// User-message serialization at the message_start / entry_appended /
// message_end seams (the former `message_json(&user.clone())` sites).
// ---------------------------------------------------------------------------

/// Inline plugin recording the user-message event payloads.
struct MessageRecordingPlugin {
    info: PluginInfo,
    seen: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
}

impl MessageRecordingPlugin {
    fn new(seen: Arc<Mutex<Vec<(String, serde_json::Value)>>>) -> Self {
        Self {
            info: PluginInfo::inline("message-recorder"),
            seen,
        }
    }
}

impl AgentPlugin for MessageRecordingPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::List(vec![
            "message_start".into(),
            "entry_appended".into(),
            "message_end".into(),
        ])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            seen.lock()
                .unwrap()
                .push((event.name.to_string(), event.payload.clone()));
            PluginEventOutcome::None
        })
    }
}

#[tokio::test]
async fn user_message_events_carry_the_message_json() {
    let temp = TempDir::new("message-json");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let seen: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: Some(session),
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![Arc::new(MessageRecordingPlugin::new(Arc::clone(&seen)))],
    ));

    agent
        .prompt("hello", event_handler(EventLog::default()))
        .await
        .expect("prompt");

    let seen = seen.lock().unwrap();
    // The plugin sees the full event surface (session_start/input/agent_settled
    // included); the message-JSON contract is about the user-message events.
    let message_events: Vec<_> = seen
        .iter()
        .filter(|(name, _)| {
            matches!(
                name.as_str(),
                "message_start" | "entry_appended" | "message_end"
            )
        })
        .collect();
    // The user-message phase emits message_start / message_end /
    // entry_appended: the extension `message_end` dispatch happens before the
    // session persists the message (upstream `_handleAgentEvent`), so the
    // stored entry follows it. The assistant turn appends its own
    // entry_appended afterwards, which is outside this contract.
    let message_events: Vec<_> = message_events.into_iter().take(3).collect();
    let kinds: Vec<&str> = message_events
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        kinds,
        vec!["message_start", "message_end", "entry_appended"],
        "{seen:?}"
    );
    for (name, payload) in message_events.iter() {
        let message = match name.as_str() {
            "entry_appended" => &payload["entry"]["message"],
            _ => &payload["message"],
        };
        assert_eq!(
            message["role"],
            serde_json::json!("user"),
            "{name}: {payload}"
        );
        assert_eq!(
            message["content"][0]["text"],
            serde_json::json!("hello"),
            "{name}: {payload}"
        );
    }
}

// ---------------------------------------------------------------------------
// `turn_end` payload (R1-L1-009): one full `contextMessages` view, unchanged
// field set/order, and persisted entry ids resolved from the turn.
// ---------------------------------------------------------------------------

/// Inline plugin recording every `turn_end` payload.
struct TurnEndRecordingPlugin {
    info: PluginInfo,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl TurnEndRecordingPlugin {
    fn new(seen: Arc<Mutex<Vec<serde_json::Value>>>) -> Self {
        Self {
            info: PluginInfo::inline("turn-end-recorder"),
            seen,
        }
    }
}

impl AgentPlugin for TurnEndRecordingPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::List(vec!["turn_end".into()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            seen.lock().unwrap().push(event.payload.clone());
            PluginEventOutcome::None
        })
    }
}

#[tokio::test]
async fn turn_end_payload_keeps_its_field_contract() {
    let temp = TempDir::new("turn-end-payload");
    let registry = fake_registry(Arc::new(|index, _context| match index {
        0 => assistant(vec![tool_call("call-1", "echo")], StopReason::ToolUse),
        _ => assistant(vec![text("done")], StopReason::Stop),
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let path = session.path().to_path_buf();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session: Some(session),
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![Arc::new(TurnEndRecordingPlugin::new(Arc::clone(&seen)))],
    ));

    agent
        .prompt("hi", event_handler(EventLog::default()))
        .await
        .expect("prompt");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "one turn_end per turn: {seen:?}");
    let tool_turn = &seen[0];
    // Field set AND order: existing handlers read these keys by name, but the
    // serialized payload order is part of the observable wire shape.
    let keys: Vec<&str> = tool_turn
        .as_object()
        .expect("turn_end payload object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "type",
            "turnIndex",
            "message",
            "toolResults",
            "toolResultEntryIds",
            "entries",
            "continue",
            "outcome",
            "context",
            "messageEntryId",
        ],
        "turn_end payload field order changed: {tool_turn}"
    );
    assert_eq!(tool_turn["type"], serde_json::json!("turn_end"));
    assert_eq!(tool_turn["message"]["role"], serde_json::json!("assistant"));
    assert_eq!(tool_turn["toolResults"].as_array().map(Vec::len), Some(1));
    let result_ids = tool_turn["toolResultEntryIds"]
        .as_array()
        .expect("toolResultEntryIds array");
    assert_eq!(
        result_ids.len(),
        1,
        "the persisted tool result id: {tool_turn}"
    );
    // `contextMessages` is the full live context preview, serialized once.
    let context_messages = tool_turn["context"]["contextMessages"]
        .as_array()
        .expect("contextMessages array");
    assert!(
        context_messages.len() >= 3,
        "expected the user, assistant and tool-result messages: {tool_turn}"
    );
    assert_eq!(
        context_messages.last().expect("last context message")["role"],
        serde_json::json!("toolResult")
    );
    assert_eq!(tool_turn["context"]["canContinue"], serde_json::json!(true));
    assert_eq!(tool_turn["entries"], serde_json::json!([]));
    assert_eq!(tool_turn["continue"], serde_json::json!(false));
    assert_eq!(tool_turn["outcome"], serde_json::json!("completed"));

    // The advertised ids name real entries in the persisted transcript.
    let reopened = SessionManager::open(&path).expect("reopen session");
    let message_entry_id = tool_turn["messageEntryId"]
        .as_str()
        .expect("messageEntryId string");
    assert!(
        reopened.entry(message_entry_id).is_some(),
        "messageEntryId {message_entry_id} is not a persisted entry"
    );
    let result_id = result_ids[0].as_str().expect("tool result id string");
    assert!(
        reopened.entry(result_id).is_some(),
        "toolResultEntryIds entry {result_id} is not a persisted entry"
    );
}

// ---------------------------------------------------------------------------
// Deferred compaction-projection materialization (per provider request)
// ---------------------------------------------------------------------------

/// The compaction threshold check runs on every provider request. When the
/// prompt-start projection snapshot is fresh it must stay BORROWED: no
/// projection/branch clone, no tail materialization — the deep copy
/// (`compaction::materialize_projected_tail`) is deferred until the threshold
/// actually decides to compact. This pins the count: two full prompts against
/// a huge context window (threshold never met) perform ZERO materializations
/// while both requests really run the hook.
#[tokio::test]
async fn sub_threshold_provider_requests_do_not_materialize_the_projection() {
    let temp = TempDir::new("compaction-defer-none");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("model reply")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let mut model = fake_model();
    // Effectively unreachable threshold: the estimate stays far below the
    // window minus the reserve.
    model.context_window = 1_000_000;
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model,
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(None),
        session: Some(session),
        compaction: Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 8,
        }),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });

    pir::compaction::reset_projection_materializations();
    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    for index in 0..2 {
        let terminal = agent
            .prompt(
                &format!("u-{index}-{}", "x".repeat(200)),
                event_handler(events.clone()),
            )
            .await
            .expect("prompt");
        assert_eq!(
            text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
            Some("model reply")
        );
    }
    assert_eq!(
        pir::compaction::projection_materializations(),
        0,
        "two sub-threshold provider requests must not materialize the projection"
    );
}

/// The deferral's other half: a threshold-triggering request performs ZERO
/// owned-basis materializations too (the borrowed `ProjectionEntryMeta` /
/// `CutEntryRef` basis replaced the owned cutoff list), and the compaction
/// still happens — the assertion below reopens the session and checks that the
/// transcript really restarted at the summary.
#[tokio::test]
async fn threshold_triggering_provider_request_materializes_the_projection_once() {
    let temp = TempDir::new("compaction-defer-once");
    let registry = fake_registry(Arc::new(|_index, context| {
        let is_summarizer = context.messages.len() == 1
            && matches!(&context.messages[0], Message::User(user)
            if user.content.iter().any(|block| matches!(
                block,
                ContentBlock::Text(text) if text.text.contains("<conversation>")
            )));
        if is_summarizer {
            assistant(vec![text("COMPACT SUMMARY")], StopReason::Stop)
        } else {
            assistant(vec![text("model reply")], StopReason::Stop)
        }
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let path = session.path().to_path_buf();
    // Tiny window + reserve 1: every request exceeds the budget.
    let mut model = fake_model();
    model.context_window = 64;
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model,
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(None),
        session: Some(session),
        compaction: Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 8,
        }),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });

    pir::compaction::reset_projection_materializations();
    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    // Warm-up prompt: the threshold is already exceeded here (nothing to
    // compact yet — the single user message is covered by the keep-recent
    // window). VERDICT (test was stale, implementation improved): the hook no
    // longer materializes an owned projection basis AT ALL — the trigger check,
    // the cut-point selection and the summarization run over per-entry
    // METADATA plus slices of the live context (`ProjectionEntryMeta` /
    // `CutEntryRef`, see `src/compaction/mod.rs`), so the counter stays 0 on
    // every request, threshold met or not.
    agent
        .prompt(
            &format!("u1-{}", "x".repeat(400)),
            event_handler(events.clone()),
        )
        .await
        .expect("warm-up prompt");
    assert_eq!(
        pir::compaction::projection_materializations(),
        0,
        "the warm-up request must not materialize an owned projection basis"
    );
    pir::compaction::reset_projection_materializations();

    let terminal = agent
        .prompt(
            &format!("u2-{}", "y".repeat(400)),
            event_handler(events.clone()),
        )
        .await
        .expect("prompt");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("model reply")
    );
    assert_eq!(
        pir::compaction::projection_materializations(),
        0,
        "even the threshold-triggering request runs on the borrowed basis"
    );

    // The compaction really happened: the transcript restarts at the summary.
    let reopened = SessionManager::open(&path).expect("reopen session");
    let entries = reopened.build_context_entries().expect("context entries");
    let rendered = entries
        .iter()
        .filter_map(text_of_message)
        .collect::<Vec<_>>();
    assert!(
        rendered.iter().any(|text| text.contains("COMPACT SUMMARY")),
        "compaction summary missing after reopen: {rendered:?}"
    );
}

// ---------------------------------------------------------------------------
// Race regressions: manual compaction vs the run/abort/queue lifecycle
// ---------------------------------------------------------------------------

/// Session-event recorder: (type, JSON) pairs.
type SessionEventLog = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

fn session_event_handler(log: SessionEventLog) -> SessionEventHandler {
    Arc::new(move |event: SessionEvent| {
        let json = session_event_to_json(&event);
        let kind = json
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        log.lock().unwrap().push((kind, json));
    })
}

/// The compaction events recorded so far (in order).
fn compaction_event_kinds(log: &SessionEventLog) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|(kind, _)| kind.starts_with("compaction_"))
        .map(|(kind, _)| kind.clone())
        .collect()
}

/// Bounded wait for a seam counter to reach `target` (the
/// `wait_for_tool_entry` pattern, generalized for the plugin/seam gates the
/// compaction-race tests park on).
async fn wait_for_counter(counter: &Arc<AtomicUsize>, target: usize, what: &str) {
    for _ in 0..2000 {
        if counter.load(Ordering::SeqCst) >= target {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("{what} never started");
}

/// Seed a session with two complete long turns (a compactable history)
/// without running the model: the race tests need entries before the cut
/// point regardless of what the gated run below does.
fn seed_compactable_history(session: &mut SessionManager) {
    for index in 0..2u64 {
        session
            .append_message(Message::User(UserMessage {
                role: UserRole::User,
                content: vec![text(&format!("seed-{index}-{}", "x".repeat(400)))],
                tools_added: None,
                tools_removed: None,
                timestamp: index + 1,
                sections: None,
            }))
            .expect("seed user message");
        session
            .append_message(Message::Assistant(assistant(
                vec![text(&format!("reply-{index}-{}", "y".repeat(400)))],
                StopReason::Stop,
            )))
            .expect("seed assistant message");
    }
}

/// Session config for the compaction-race tests: a huge context window keeps
/// the automatic threshold hook idle, so only the MANUAL compaction path
/// under test runs.
fn race_agent_config(
    registry: Arc<ProviderRegistry>,
    session: SessionManager,
    temp: &TempDir,
    gate: Option<(tokio::sync::watch::Receiver<bool>, Arc<AtomicUsize>)>,
) -> AgentSessionConfig {
    let mut model = fake_model();
    model.context_window = 1_000_000;
    AgentSessionConfig {
        provider_registry: registry,
        model,
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(gate),
        session: Some(session),
        compaction: Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 8,
        }),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

/// Fake provider for the race tests: the first model turn issues the `gate`
/// tool call, later model turns answer `final_text`, and every summarizer
/// request (the upstream SUMMARIZATION_SYSTEM_PROMPT) returns
/// The compactor composes the main summary with upstream's split-turn
/// continuation section when the cut point splits a turn (the transcript these
/// tests compact is mid-turn by construction); the mock summarizer answers both
/// calls with the same text, so the composed result is exactly this.
fn assert_split_turn_summary(summary: &str) {
    assert_eq!(
        summary, "COMPACT SUMMARY\n\n---\n\n**Turn Context (split turn):**\n\nCOMPACT SUMMARY",
        "compaction must compose the main summary + the split-turn context section, once"
    );
}

/// `COMPACT SUMMARY`.
fn race_registry(final_text: &'static str) -> Arc<ProviderRegistry> {
    let model_turns = Arc::new(AtomicUsize::new(0));
    fake_registry(Arc::new(move |_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("COMPACT SUMMARY")], StopReason::Stop);
        }
        if model_turns.fetch_add(1, Ordering::SeqCst) == 0 {
            assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
        } else {
            assistant(vec![text(final_text)], StopReason::Stop)
        }
    }))
}

/// Upstream `compact()` opens with `await this.abort()`: a manual `/compact`
/// issued while the model is streaming STOPS the response and then compacts.
/// The streamed turn settles aborted, the compaction emits exactly one manual
/// start/end pair, and no prompt runs concurrently with the compaction.
#[tokio::test]
async fn compact_while_streaming_aborts_the_run_and_then_compacts() {
    let temp = TempDir::new("compact-while-streaming");
    let (_gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    let agent = Arc::new(AgentSession::new(race_agent_config(
        race_registry("done"),
        session,
        &temp,
        Some((gate_rx, Arc::clone(&entered))),
    )));
    let session_log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_handler(Arc::clone(&session_log)));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("start", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_tool_entry(&entered).await;
    assert!(agent.is_streaming(), "the run holds the session claim");

    let result = agent
        .compact(None)
        .await
        .expect("compact aborts the streaming run and then compacts");
    assert_split_turn_summary(&result.summary);
    assert_eq!(
        compaction_event_kinds(&session_log),
        vec!["compaction_start".to_owned(), "compaction_end".to_owned()],
        "the abort-then-compact path emits one manual pair: {session_log:?}"
    );

    // The aborted run settles on its own; its claim was what `compact` waited
    // for.
    let terminal = prompt_task
        .await
        .expect("prompt task")
        .expect("prompt result");
    assert_eq!(terminal.stop_reason, StopReason::ToolUse);
    assert!(!agent.is_streaming(), "the run released the claim");
    assert!(!agent.is_compacting());
}

/// An abort followed IMMEDIATELY by `compact()` (the Esc-then-`/compact`
/// flow) must not fail with the bogus streaming error: the abort is recorded
/// as a run abort request, `compact()` waits for the run's claim to release
/// (upstream `compact()` opens with `await this.abort()`), and then the
/// compaction runs.
#[tokio::test]
async fn compact_after_abort_waits_for_the_run_and_succeeds() {
    let temp = TempDir::new("compact-after-abort");
    let (_gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    let agent = Arc::new(AgentSession::new(race_agent_config(
        race_registry("done"),
        session,
        &temp,
        Some((gate_rx, Arc::clone(&entered))),
    )));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("start", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_tool_entry(&entered).await;

    // No await between the abort and the compact: on the current-thread
    // runtime the run task has not observed the abort yet, so the session is
    // still claimed RUNNING when `compact()` starts — exactly the window that
    // used to reject with "cannot compact while the agent is streaming".
    agent.abort();
    assert!(
        agent.is_streaming(),
        "the aborted run is still winding down here"
    );
    let result = agent
        .compact(None)
        .await
        .expect("compact must wait for the aborting run, not fail");
    assert_split_turn_summary(&result.summary);

    // The run settled on its abort after the tool batch.
    let terminal = prompt_task
        .await
        .expect("prompt task")
        .expect("prompt result");
    assert_eq!(terminal.stop_reason, StopReason::ToolUse);
    assert!(!agent.is_streaming());
    assert!(!agent.is_compacting());
}

/// A queued prompt + abort + compact interleaving: the queued message is not
/// dropped by the abort or the compaction, the compaction succeeds while the
/// aborted run winds down, and the next run delivers the queued message in
/// its first request. The compaction must not deadlock against the run
/// (upstream keeps both queues across abort/compact).
#[tokio::test]
async fn queued_message_survives_abort_and_compaction_and_runs_next_turn() {
    let temp = TempDir::new("queue-abort-compact");
    let (_gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let observed_for_responder = Arc::clone(&observed);
    let model_turns = Arc::new(AtomicUsize::new(0));
    let turns = Arc::clone(&model_turns);
    let registry = fake_registry(Arc::new(move |_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("COMPACT SUMMARY")], StopReason::Stop);
        }
        if turns.fetch_add(1, Ordering::SeqCst) == 0 {
            return assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse);
        }
        observed_for_responder
            .lock()
            .unwrap()
            .push(user_texts(context));
        assistant(vec![text("next reply")], StopReason::Stop)
    }));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    let agent = Arc::new(AgentSession::new(race_agent_config(
        registry,
        session,
        &temp,
        Some((gate_rx, Arc::clone(&entered))),
    )));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("start", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_tool_entry(&entered).await;

    // Queue a steering prompt during the run, then abort AND compact without
    // waiting: the compaction claims the session only after the aborted run
    // releases it, and the claim keeps a new run from starting mid-compaction.
    assert_eq!(agent.steer("queued redirect"), Disposition::Queued);
    agent.abort();
    let result = agent
        .compact(None)
        .await
        .expect("compact after abort with a queued message");
    assert_split_turn_summary(&result.summary);
    let terminal = prompt_task
        .await
        .expect("prompt task")
        .expect("prompt result");
    assert_eq!(terminal.stop_reason, StopReason::ToolUse);
    assert!(!agent.is_streaming());

    // The queued message survived both the abort and the compaction.
    assert_eq!(
        agent.get_steering_messages().await,
        vec!["queued redirect".to_owned()]
    );

    // The next run delivers it in its first request.
    agent
        .prompt("next", event_handler(EventLog::default()))
        .await
        .expect("next prompt");
    let seen = observed.lock().unwrap();
    assert!(
        seen.iter()
            .any(|request| request.iter().any(|text| text == "queued redirect")),
        "queued message never reached the model: {seen:?}"
    );
}

/// A run terminated by a provider error also releases the session claim: a
/// manual compaction succeeds right after, and a new prompt can start.
#[tokio::test]
async fn provider_error_run_releases_the_streaming_claim() {
    let temp = TempDir::new("error-run-claim");
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("COMPACT SUMMARY")], StopReason::Stop);
        }
        let mut message = assistant(Vec::new(), StopReason::Error);
        message.error_message = Some("connection reset by peer".into());
        message
    }));
    let agent = Arc::new(AgentSession::new(race_agent_config(
        registry, session, &temp, None,
    )));

    let terminal = agent
        .prompt("hi", event_handler(EventLog::default()))
        .await
        .expect("the error terminal still returns Ok");
    assert_eq!(terminal.stop_reason, StopReason::Error);
    assert!(!agent.is_streaming(), "the error run released the claim");

    let result = agent
        .compact(None)
        .await
        .expect("compact after the error-terminated run");
    assert_split_turn_summary(&result.summary);

    // A new prompt starts (the claim is not stuck).
    let terminal = agent
        .prompt("again", event_handler(EventLog::default()))
        .await
        .expect("prompt after the error run");
    assert_eq!(terminal.stop_reason, StopReason::Error);
    assert!(!agent.is_streaming());
}

/// Dropping the run future mid-await (a host's `prompt_task.abort()`, the
/// exit path that never runs post-await cleanup) releases the session claim
/// through the RAII guard: compaction succeeds and a later prompt starts
/// instead of the session being wedged as "already streaming" forever.
#[tokio::test]
async fn dropped_prompt_task_releases_the_streaming_claim() {
    let temp = TempDir::new("dropped-run-claim");
    let (_gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    let agent = Arc::new(AgentSession::new(race_agent_config(
        race_registry("after drop"),
        session,
        &temp,
        Some((gate_rx, Arc::clone(&entered))),
    )));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("start", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_tool_entry(&entered).await;
    assert!(agent.is_streaming());

    prompt_task.abort();
    let cancelled = prompt_task.await;
    assert!(cancelled.is_err(), "the prompt task must be cancelled");
    assert!(
        !agent.is_streaming(),
        "dropping the run future must release the claim"
    );

    let result = agent
        .compact(None)
        .await
        .expect("compact after the dropped run");
    assert_split_turn_summary(&result.summary);

    let terminal = agent
        .prompt("after", event_handler(EventLog::default()))
        .await
        .expect("prompt after the dropped run");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("after drop")
    );
}

/// Inline plugin that parks one event's dispatch on a gate, so a test can
/// hold a run/compaction in a known phase deterministically: the plugin
/// dispatch is awaited inside the session (`session_before_compact` before
/// the summarizer call, `agent_settled` in the run's post-loop settlement).
struct GatePlugin {
    info: PluginInfo,
    event: &'static str,
    entered: Arc<AtomicUsize>,
    gate: tokio::sync::watch::Receiver<bool>,
}

impl AgentPlugin for GatePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::List(vec![self.event.into()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        if event.name != self.event {
            return Box::pin(async { PluginEventOutcome::None });
        }
        let entered = Arc::clone(&self.entered);
        let mut gate = self.gate.clone();
        Box::pin(async move {
            entered.fetch_add(1, Ordering::SeqCst);
            // Bounded wait: a test that never releases the gate must fail
            // instead of hanging the suite.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if *gate.borrow() {
                        break;
                    }
                    if gate.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
            PluginEventOutcome::None
        })
    }
}

/// Inline plugin recording selected plugin events (name + payload) without
/// parking dispatch, so tests can assert the terminal failure event that the
/// host emits alongside the session event stream.
struct PluginEventRecorder {
    info: PluginInfo,
    events: Vec<String>,
    seen: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
}

impl PluginEventRecorder {
    #[allow(clippy::type_complexity)]
    fn new(name: &str, events: &[&str]) -> (Self, Arc<Mutex<Vec<(String, serde_json::Value)>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                info: PluginInfo::inline(name),
                events: events.iter().map(|event| (*event).to_owned()).collect(),
                seen: Arc::clone(&seen),
            },
            seen,
        )
    }
}

impl AgentPlugin for PluginEventRecorder {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::List(self.events.clone())
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        if !self
            .events
            .iter()
            .any(|watched| watched.as_str() == event.name.as_ref())
        {
            return Box::pin(async { PluginEventOutcome::None });
        }
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            seen.lock()
                .unwrap()
                .push((event.name.to_string(), event.payload.clone()));
            PluginEventOutcome::None
        })
    }
}

/// A prompt submitted while a MANUAL COMPACTION is in flight is rejected with
/// the upstream message — the compaction claim is the mutual exclusion, so a
/// run can never start mid-compaction (and a queued prompt is handled by the
/// host's queue, never by racing the compaction).
#[tokio::test]
async fn prompt_while_compacting_is_rejected_like_upstream() {
    let temp = TempDir::new("prompt-while-compacting");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    // No gate tool here: every model turn answers directly, so the post-
    // compaction prompt completes in one turn.
    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("COMPACT SUMMARY")], StopReason::Stop);
        }
        assistant(vec![text("late")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new_with_plugins(
        race_agent_config(registry, session, &temp, None),
        vec![Arc::new(GatePlugin {
            info: PluginInfo::inline("compact-gate"),
            event: "session_before_compact",
            entered: Arc::clone(&entered),
            gate: gate_rx,
        })],
    ));

    let compact_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move { agent.compact(None).await }
    });
    wait_for_counter(&entered, 1, "session_before_compact gate").await;
    assert!(agent.is_compacting(), "the compaction holds the claim");
    assert!(!agent.is_streaming());

    let rejected = agent
        .prompt("blocked", event_handler(EventLog::default()))
        .await;
    let error = rejected.expect_err("a prompt during compaction must be rejected");
    assert_eq!(
        error.to_string(),
        "Cannot submit a prompt while compaction is in progress. \
         Wait for compaction to finish and retry."
    );

    let _ = gate_tx.send(true);
    let result = compact_task.await.expect("compact task").expect("compact");
    assert_split_turn_summary(&result.summary);
    assert!(!agent.is_compacting());

    // Idle again: the prompt is accepted.
    let terminal = agent
        .prompt("after", event_handler(EventLog::default()))
        .await
        .expect("prompt after the compaction");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("late")
    );
}

/// FIX-A04 (R1-L1-003): a stale manual-compaction claim's `Drop` must not
/// clear a NEWER compaction's claim. The first compaction fails and parks in
/// its `session_compact_failed` dispatch (its explicit release has already
/// returned the session to idle). While it is parked, a second compaction
/// claims the session and parks at `session_before_compact` still holding
/// COMPACTING. Releasing the first then runs its claim guard's `Drop`; the
/// owner check must make it a no-op against the second claim.
#[tokio::test]
async fn stale_manual_compaction_claim_does_not_clear_a_newer_compaction() {
    let temp = TempDir::new("stale-compact-claim");
    let (before_tx, before_rx) = tokio::sync::watch::channel(false);
    let (failed_tx, failed_rx) = tokio::sync::watch::channel(false);
    let before_entered = Arc::new(AtomicUsize::new(0));
    let failed_entered = Arc::new(AtomicUsize::new(0));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);

    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            let mut message = assistant(Vec::new(), StopReason::Error);
            message.error_message = Some("summarizer exploded".into());
            return message;
        }
        assistant(vec![text("late")], StopReason::Stop)
    }));

    let agent = Arc::new(AgentSession::new_with_plugins(
        race_agent_config(registry, session, &temp, None),
        vec![
            Arc::new(GatePlugin {
                info: PluginInfo::inline("before-compact-gate"),
                event: "session_before_compact",
                entered: Arc::clone(&before_entered),
                gate: before_rx,
            }),
            Arc::new(GatePlugin {
                info: PluginInfo::inline("failed-compact-gate"),
                event: "session_compact_failed",
                entered: Arc::clone(&failed_entered),
                gate: failed_rx,
            }),
        ],
    ));

    // First compaction: fail the summarizer call, then park in the failed
    // event dispatch AFTER the claim's explicit release.
    let first = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move { agent.compact(None).await }
    });
    wait_for_counter(&before_entered, 1, "first session_before_compact").await;
    let _ = before_tx.send(true);
    wait_for_counter(&failed_entered, 1, "first session_compact_failed").await;
    assert!(
        !agent.is_compacting(),
        "the first claim is released before its failed event is dispatched"
    );

    // Re-arm the before gate and let a second compaction claim the session.
    // It parks in `session_before_compact` while still holding COMPACTING.
    let _ = before_tx.send(false);
    let second = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move { agent.compact(None).await }
    });
    wait_for_counter(&before_entered, 2, "second session_before_compact").await;
    assert!(
        agent.is_compacting(),
        "the second compaction must hold the claim"
    );
    let rejected = agent
        .prompt("blocked", event_handler(EventLog::default()))
        .await;
    assert_eq!(
        rejected
            .expect_err("a prompt during the second compaction must be rejected")
            .to_string(),
        "Cannot submit a prompt while compaction is in progress. \
         Wait for compaction to finish and retry."
    );

    // Release the first failed event: its claim guard drops while the second
    // compaction is still active. The stale Drop must not touch the new claim.
    let _ = failed_tx.send(true);
    assert!(
        first.await.expect("first compact task").is_err(),
        "the first compaction failed as scripted"
    );
    assert!(
        agent.is_compacting(),
        "a stale claim's Drop must not clear the newer compaction"
    );
    let rejected = agent
        .prompt("blocked", event_handler(EventLog::default()))
        .await;
    assert_eq!(
        rejected
            .expect_err("the newer compaction still holds the session")
            .to_string(),
        "Cannot submit a prompt while compaction is in progress. \
         Wait for compaction to finish and retry."
    );

    // Release the second compaction: it fails the same way and returns to idle.
    let _ = before_tx.send(true);
    assert!(
        second.await.expect("second compact task").is_err(),
        "the second compaction failed as scripted"
    );
    assert!(
        !agent.is_compacting(),
        "the live claim releases the session when its compaction ends"
    );
}

/// The user-visible race the report names: the run produced its final output
/// (the agent loop already reached its terminal result) but its future is
/// still in the post-run settlement window. A `/compact` issued there used to
/// be rejected as "cannot compact while the agent is streaming" although no
/// output is streaming anymore. The fix publishes the settle transition and
/// `compact()` waits for the run's claim to release, then compacts.
#[tokio::test]
async fn compact_after_output_ends_waits_for_the_settlement_window() {
    let temp = TempDir::new("compact-during-settlement");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    // The run itself needs no gate tool: the gated `agent_settled` plugin
    // dispatch below parks the run's settlement phase deterministically.
    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("COMPACT SUMMARY")], StopReason::Stop);
        }
        assistant(vec![text("done")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new_with_plugins(
        race_agent_config(registry, session, &temp, None),
        vec![Arc::new(GatePlugin {
            info: PluginInfo::inline("settle-gate"),
            event: "agent_settled",
            entered: Arc::clone(&entered),
            gate: gate_rx,
        })],
    ));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("start", event_handler(EventLog::default()))
                .await
        }
    });
    // The loop ended (final output produced); only the settlement dispatch
    // remains, parked on the gate.
    wait_for_counter(&entered, 1, "agent_settled gate").await;
    assert!(
        agent.is_streaming(),
        "the run's future is still alive in its settlement window"
    );

    // `compact()` must WAIT for the settlement instead of rejecting: the
    // stream is over, only post-run bookkeeping is running. Poll the pinned
    // future exactly once — with the run parked on the gate it cannot have
    // settled, so the first poll MUST park (the pre-fix code returned the
    // bogus streaming error right here).
    let mut compact = Box::pin(agent.compact(None));
    // Wrap the inner `Poll` in `Ready` so the await completes immediately
    // with the observed poll result instead of leaving the wrapper pending.
    let first_poll = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(compact.as_mut(), cx))
    })
    .await;
    assert!(
        first_poll.is_pending(),
        "compact during the settlement window must park waiting for the run, got {first_poll:?}"
    );

    // Release the settlement gate: the run's future finishes and hands the
    // claim over; the parked compaction then completes.
    let _ = gate_tx.send(true);
    let result = compact
        .await
        .expect("compact completes once the run settles");
    assert_split_turn_summary(&result.summary);

    let terminal = prompt_task
        .await
        .expect("prompt task")
        .expect("prompt result");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal.clone())).as_deref(),
        Some("done")
    );
    assert!(!agent.is_streaming());
}

// ---------------------------------------------------------------------------
// D6: the system prompt follows a RUNTIME active-set change
// ---------------------------------------------------------------------------

/// Hands the test the live [`PluginContext`] bound at construction, which is
/// the same `SessionService` surface a JS extension's `pi.setActiveTools` and
/// the native builtins write through.
struct ActiveSetHandle {
    info: PluginInfo,
    context: Mutex<Option<pir::plugins::plugin::PluginContext>>,
}

impl AgentPlugin for ActiveSetHandle {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn on_context(&self, ctx: &pir::plugins::plugin::PluginContext) {
        *self.context.lock().unwrap() = Some(ctx.clone());
    }
}

/// D6: `setActiveTools` (and therefore `tool_search`'s load and the MCP
/// discovery activation, which write the same override cell) must rebuild the
/// context-derived prompt — upstream re-runs `_rebuildSystemPrompt` on every
/// active-set change (`agent-session.ts:1532-1536`, `:1705-1731`). Before the
/// fix only the build and `/reload` rebuilt the prompt, so the model kept
/// seeing the startup tool list after the runtime changed the declared set.
///
/// The rebuild is driven from [`AgentSession::build_tool_loadout`], so one
/// settle iteration is the observable point: a prompt run then a snapshot.
#[tokio::test]
async fn system_prompt_follows_a_runtime_active_set_change() {
    // A clean agent dir keeps the discovery hermetic: the cwd's AGENTS.md is
    // the only context input beyond the shared process environment.
    let _env = crate::common::AgentDirEnvGuard::scratch("d6-runtime", None).await;
    let temp = TempDir::new("d6-runtime");
    std::fs::write(temp.0.join("AGENTS.md"), "project instructions").expect("write AGENTS.md");

    let mut tools = ToolRegistry::new();
    pir::coding_agent::register_core_tools(&mut tools);
    let handle = Arc::new(ActiveSetHandle {
        info: PluginInfo::inline("d6-active-set"),
        context: Mutex::new(None),
    });
    // Capture the system prompt of every request the run sends: the same
    // settle iteration that adopts the new active set must send the rebuilt
    // prompt, not the startup one.
    let seen_prompts: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_for_provider = Arc::clone(&seen_prompts);
    let registry = fake_registry(Arc::new(move |_index, context| {
        seen_for_provider
            .lock()
            .unwrap()
            .push(context.system_prompt.clone());
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let session = AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: Arc::clone(&registry),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session: None,
            compaction: None,
            max_turns: 2,
            // Let the context builder own the prompt.
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: Some(registry_stream_override(registry, fake_model())),
            extensions: None,
            tool_selection: None,
        },
        vec![handle.clone()],
    );

    let before = session
        .system_prompt_snapshot()
        .expect("the context file produces the context-derived prompt");
    assert!(
        before.contains("- read: Read file contents"),
        "the startup prompt lists the default active tools: {before}"
    );
    assert!(
        before.contains("- bash: Execute bash commands"),
        "the startup prompt lists bash: {before}"
    );

    // The runtime write: exactly what `pi.setActiveTools(["read"])` does.
    let ctx = handle
        .context
        .lock()
        .unwrap()
        .clone()
        .expect("the plugin host bound the live service context");
    let read_only: Vec<Arc<str>> = vec![Arc::from("read")];
    ctx.session()
        .expect("the live session service is bound")
        .set_active_tools(&read_only);

    // One settle iteration builds the loadout (and with it the prompt).
    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    session
        .prompt("go", event_handler(events))
        .await
        .expect("prompt");

    // The same settle iteration's request already carries the rebuilt prompt
    // (the per-iteration refresh after `build_tool_loadout`).
    let request_prompt = seen_prompts
        .lock()
        .unwrap()
        .first()
        .cloned()
        .flatten()
        .expect("the run sends its system prompt");
    assert!(
        request_prompt.contains("- read: Read file contents"),
        "the request advertises the new active set: {request_prompt}"
    );
    assert!(
        !request_prompt.contains("- bash: Execute bash commands"),
        "the request no longer advertises the deactivated tool: {request_prompt}"
    );

    let after = session
        .system_prompt_snapshot()
        .expect("the prompt stays context-derived");
    assert!(
        after.contains("- read: Read file contents"),
        "the newly declared tool stays advertised: {after}"
    );
    assert!(
        !after.contains("- bash: Execute bash commands"),
        "the deactivated tool must leave the prompt without a /reload: {after}"
    );
    assert!(
        !after.contains("Use bash for file operations like ls, rg, find"),
        "the rules section follows the same declared set: {after}"
    );
}

/// P7: a session on a BARE workspace (no `AGENTS.md`/`CLAUDE.md`, no skills,
/// nothing to inject) must still send the base system prompt — upstream
/// `buildSystemPrompt` (`system-prompt.ts:60-199`) has no "no resources → no
/// prompt" arm, it always renders the preamble, the `<tools>` list, the
/// `<rules>` and `<cwd>`.
///
/// Before the fix the session build shared print's "nothing to inject → `None`"
/// rule, so the TUI/RPC/server sent no system prompt at all on a fresh project:
/// the model never saw the tool list or the rules.
#[tokio::test]
async fn bare_workspace_session_still_builds_the_base_prompt() {
    // A clean agent dir AND a clean home keep discovery hermetic: otherwise a
    // user-global `~/.agents/skills` on the machine would make the prompt
    // non-empty for the wrong reason and the test could pass without the fix.
    let env_home = TempDir::new("bare-prompt-home");
    let agent_dir = env_home.0.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    let home = env_home.0.to_string_lossy().into_owned();
    let _env =
        crate::common::AgentDirEnvGuard::at(&agent_dir, &[("HOME", &home), ("USERPROFILE", &home)])
            .await;
    let temp = TempDir::new("bare-prompt");
    assert!(
        !temp.0.join("AGENTS.md").exists() && !temp.0.join("CLAUDE.md").exists(),
        "the workspace must be bare for this test to mean anything"
    );

    let seen_prompts: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_for_provider = Arc::clone(&seen_prompts);
    let registry = fake_registry(Arc::new(move |_index, context| {
        seen_for_provider
            .lock()
            .unwrap()
            .push(context.system_prompt.clone());
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let mut tools = ToolRegistry::new();
    pir::coding_agent::register_core_tools(&mut tools);
    let session = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::clone(&registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: None,
        compaction: None,
        max_turns: 2,
        // No explicit override: the context builder owns the prompt.
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: Some(registry_stream_override(registry, fake_model())),
        extensions: None,
        tool_selection: None,
    });

    let prompt = session
        .system_prompt_snapshot()
        .expect("a bare-workspace session still gets the base prompt");
    assert!(
        !prompt.trim().is_empty(),
        "the base prompt must not be empty"
    );
    assert!(
        prompt.contains("<tools>") && prompt.contains("\n- read: Read file contents"),
        "the prompt advertises the declared tools: {prompt}"
    );
    assert!(
        prompt.contains("<rules>"),
        "the prompt carries the rules section: {prompt}"
    );
    assert!(
        prompt.contains("<cwd>"),
        "the prompt carries the working directory: {prompt}"
    );

    // The run sends it too (not just the snapshot accessor).
    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    session
        .prompt("go", event_handler(events))
        .await
        .expect("prompt");
    let request_prompt = seen_prompts
        .lock()
        .unwrap()
        .first()
        .cloned()
        .flatten()
        .expect("the request carries the base prompt");
    assert!(
        request_prompt.contains("<tools>")
            && request_prompt.contains("\n- read: Read file contents"),
        "the request advertises the declared tools: {request_prompt}"
    );
}

/// A caller-supplied `system_prompt` still wins over the context builder
/// (P7 must not regress the explicit override).
#[tokio::test]
async fn explicit_system_prompt_override_is_not_replaced_by_the_base_prompt() {
    let _env = crate::common::AgentDirEnvGuard::scratch("override-prompt", None).await;
    let temp = TempDir::new("override-prompt");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let mut tools = ToolRegistry::new();
    pir::coding_agent::register_core_tools(&mut tools);
    let mut session = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::clone(&registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: Some("caller supplied".into()),
        cwd: Some(temp.0.clone()),
        event_stream: Some(registry_stream_override(registry, fake_model())),
        extensions: None,
        tool_selection: None,
    });
    assert_eq!(
        session.system_prompt_snapshot().as_deref(),
        Some("caller supplied")
    );

    // Active-set changes must not overwrite an explicit override either.
    session.set_prompt_resources(Vec::new(), Vec::new());
    assert_eq!(
        session.system_prompt_snapshot().as_deref(),
        Some("caller supplied")
    );
}

// ---------------------------------------------------------------------------
// R1 (CL38): the `ctx.modelRegistry.complete` host op never serializes the
// internal retry-class carrier
// ---------------------------------------------------------------------------

/// The host completion (`__pir_host.modelCompleteJson` → `CompleteOk {ok,
/// message}`) is PRODUCTION-wired through `push_session_context` and
/// serializes the completion's `AssistantMessage` straight to extension code.
/// That is a consumer face the agent loop's take never reaches: a completion
/// runs outside any turn, so nothing takes the carrier a failed provider
/// finalizer attached. The session host wraps the pushed closure with
/// `strip_turn_retry_class`, so a FAILED completion reaches the extension
/// carrier-free while its real error text is unchanged.
///
/// Driven end to end: a real `AgentSession` pushes the completion capability
/// into a real `ExtensionHandle`, and the extension calls
/// `ctx.modelRegistry.complete(...)` — the same host op the
/// `ModelRuntime`/`completeSimple` faces ride. The provider registry is a
/// scripted fake whose `stream` returns the classified terminal error.
#[tokio::test(flavor = "current_thread")]
async fn model_complete_json_payload_never_carries_the_retry_class() {
    use pir::ai::error::{AiError, RETRY_CLASS_DIAGNOSTIC};

    let dir = std::env::temp_dir().join(format!(
        "pir-agent-session-complete-carrier-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("ext.ts");
    std::fs::write(
        &path,
        r#"
        export default function (pi) {
            pi.registerCommand("complete-probe", {
                handler: async (_args, ctx) => {
                    const model = ctx.model;
                    let out;
                    try {
                        const message = await ctx.modelRegistry.complete(model, { messages: [] }, {});
                        out = { ok: true, message: JSON.stringify(message) };
                    } catch (error) {
                        out = { ok: false, error: String((error && error.message) || error) };
                    }
                    globalThis.__completeProbe = JSON.stringify(out);
                },
            });
        }
        "#,
    )
    .expect("write fixture");
    let handle = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
        cwd: dir.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime");
    let loaded = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    // The provider answers every request with the classified terminal error
    // (the real adapter seam's construction: text + structural verdict).
    struct FailingProvider;
    impl Provider for FailingProvider {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Failing provider"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![fake_model()]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            Vec::new()
        }
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let message = pir::ai::api::terminal_error_message(
                &fake_model(),
                &AiError::StreamNotTerminated,
                None,
            );
            assert!(
                message
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.diag_type == RETRY_CLASS_DIAGNOSTIC),
                "precondition: the finalizer attached the carrier"
            );
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::error_event(
                ErrorReason::Error,
                message,
            ));
            stream
        }
    }
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FailingProvider));

    // The session pushes the completion capability on `session_start`.
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(dir.clone()),
        event_stream: None,
        extensions: Some(handle.clone()),
        tool_selection: None,
    });
    // The session pushes the completion capability (a pre-run push is a
    // cached no-op when nothing moved, so the result is not asserted).
    agent.push_session_context_for_test().await;

    handle
        .dispatch_command("complete-probe", "")
        .await
        .expect("dispatch the completion probe");
    let probe = handle
        .eval_json("globalThis.__completeProbe")
        .await
        .expect("read probe");
    let message = probe["message"]
        .as_str()
        .unwrap_or_else(|| panic!("the host completion produced a message: {probe}"));
    assert!(
        !message.contains(RETRY_CLASS_DIAGNOSTIC),
        "the modelCompleteJson message must not carry the carrier: {message}"
    );
    assert!(
        !message.contains("retryable_turn") && !message.contains("non_retryable"),
        "nor a verdict spelling: {message}"
    );
    assert!(
        message.contains("stream ended without a terminal event"),
        "the real error text survives the strip: {message}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// provider_stream_event is NOT dispatched from the `ctx.modelRegistry.complete`
// host op (upstream wires `onProviderStreamEvent` only into the agent's
// streamSimple path)
// ---------------------------------------------------------------------------

/// The host completion runs INSIDE the extension actor. Dispatching
/// `provider_stream_event` back into that actor would re-enter the suspended
/// host op (and the awaited dispatch would wedge until the call timeout), so
/// the host-op request omits the hook entirely — matching upstream, whose
/// `ModelRegistry.complete` options carry no `onProviderStreamEvent`
/// (sdk.ts:388-426). The completion itself still succeeds.
#[tokio::test(flavor = "current_thread")]
async fn model_complete_host_op_does_not_emit_provider_stream_events() {
    let dir = std::env::temp_dir().join(format!(
        "pir-agent-session-complete-stream-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("ext.ts");
    std::fs::write(
        &path,
        r#"
        export default function (pi) {
            globalThis.__pse = 0;
            pi.on("provider_stream_event", () => {
                globalThis.__pse = (globalThis.__pse ?? 0) + 1;
            });
            pi.registerCommand("complete-probe", {
                handler: async (_args, ctx) => {
                    const model = ctx.model;
                    const message = await ctx.modelRegistry.complete(model, { messages: [] }, {});
                    const text = message.content?.[0]?.text ?? null;
                    globalThis.__completeProbe = JSON.stringify({ text });
                },
            });
        }
        "#,
    )
    .expect("write fixture");
    let handle = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
        cwd: dir.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime");
    let loaded = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    /// Emits one parsed provider event through the request's hook (when the
    /// request carries one) and completes successfully.
    struct HookProbeCompletionProvider {
        hook_calls: Arc<AtomicUsize>,
    }
    impl Provider for HookProbeCompletionProvider {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Hook probe provider"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![fake_model()]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            Vec::new()
        }
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            if let Some(hook) = &options.hooks.on_provider_stream_event {
                self.hook_calls.fetch_add(1, Ordering::SeqCst);
                hook(&serde_json::json!({ "type": "delta", "index": 0 }));
            }
            let message = AssistantMessage {
                api: "fake-api".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "completed".into(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                timestamp: 1,
                ..Default::default()
            };
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        }
    }

    let hook_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(HookProbeCompletionProvider {
        hook_calls: Arc::clone(&hook_calls),
    }));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(dir.clone()),
        event_stream: None,
        extensions: Some(handle.clone()),
        tool_selection: None,
    });
    agent.push_session_context_for_test().await;

    handle
        .dispatch_command("complete-probe", "")
        .await
        .expect("dispatch the completion probe");
    let probe = handle
        .eval_json("globalThis.__completeProbe")
        .await
        .expect("read probe");
    assert_eq!(
        probe["text"],
        serde_json::json!("completed"),
        "the host completion succeeded: {probe}"
    );
    let stream_events = handle
        .eval_json("JSON.stringify(globalThis.__pse ?? 0)")
        .await
        .expect("read provider stream count");
    assert_eq!(
        stream_events,
        serde_json::json!(0),
        "the host-op completion must not dispatch provider_stream_event"
    );
    assert_eq!(
        hook_calls.load(Ordering::SeqCst),
        0,
        "the host-op request carries no provider_stream_event hook"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// H4b: starting-message lifecycle (agent_start → turn_start →
// message_start/message_end, replacement applied before persistence)
// ---------------------------------------------------------------------------

/// Load an inline JS extension for the message-lifecycle tests below.
async fn load_message_lifecycle_extension(
    label: &str,
    source: &str,
) -> pir::extensions::ExtensionHandle {
    let dir = std::env::temp_dir().join(format!(
        "pir-agent-session-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("ext.ts");
    std::fs::write(&path, source).expect("write fixture");
    let handle = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
        cwd: dir.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime");
    let loaded = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    handle
}

/// H4b: the starting user message goes through the SAME extension
/// `message_start`/`message_end` lifecycle as every loop-produced message,
/// AFTER `agent_start` and `turn_start` (upstream `runAgentLoop`,
/// agent-loop.ts:126-131). A `message_end` replacement is what the first
/// provider request and the durable transcript record.
#[tokio::test]
async fn starting_user_message_end_replacement_enters_the_request_and_storage() {
    let temp = TempDir::new("start-user-replace");
    let handler = load_message_lifecycle_extension(
        "start-user-replace",
        r#"
        export default function (pi) {
            globalThis.__order = [];
            globalThis.__appended = [];
            pi.on("agent_start", () => globalThis.__order.push("agent_start"));
            pi.on("turn_start", () => globalThis.__order.push("turn_start"));
            pi.on("message_start", (event) => {
                globalThis.__order.push("message_start:" + (event.message ? event.message.role : ""));
            });
            pi.on("message_end", (event) => {
                globalThis.__order.push("message_end:" + (event.message ? event.message.role : ""));
                if (event.message && event.message.role === "user") {
                    const text = (event.message.content || []).map((block) => block.text || "").join("");
                    if (text === "original") {
                        return {
                            message: { ...event.message, content: [{ type: "text", text: "replaced" }] },
                        };
                    }
                }
            });
            pi.on("entry_appended", (event) => {
                const message = event.entry && event.entry.message;
                if (message && message.role === "user") {
                    globalThis.__appended.push(
                        (message.content || []).map((block) => block.text || "").join(""),
                    );
                }
            });
        }
        "#,
    )
    .await;

    let requests: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_responder = Arc::clone(&requests);
    let registry = fake_registry(Arc::new(move |_index, context| {
        requests_for_responder.lock().unwrap().push(context.clone());
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let path = session.path().to_path_buf();
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(handler.clone()),
        tool_selection: None,
    });
    let log = EventLog::default();
    agent
        .prompt("original", event_handler(Arc::clone(&log)))
        .await
        .expect("prompt");

    // The extension observed the initial-message lifecycle in upstream order,
    // and its `message_end` replacement is what it saw persisted.
    let probe = handler
        .eval_json(
            "JSON.stringify({ order: globalThis.__order ?? [], appended: globalThis.__appended ?? [] })",
        )
        .await
        .expect("probe");
    let order: Vec<String> = serde_json::from_value(probe["order"].clone()).expect("order");
    assert_eq!(
        order.iter().take(4).cloned().collect::<Vec<_>>(),
        vec![
            "agent_start".to_owned(),
            "turn_start".to_owned(),
            "message_start:user".to_owned(),
            "message_end:user".to_owned(),
        ],
        "agent_start → turn_start → message_start → message_end: {order:?}"
    );
    let appended: Vec<String> =
        serde_json::from_value(probe["appended"].clone()).expect("appended");
    assert!(!appended.is_empty(), "the user entry must be persisted");
    assert!(
        appended.iter().all(|text| text == "replaced"),
        "the persisted user entry carries the replacement: {appended:?}"
    );

    // The first provider request carries the replacement, never the original.
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "one provider request");
    assert_eq!(
        user_texts(&requests[0]),
        vec!["replaced".to_owned()],
        "the replacement is the request's user message: {:?}",
        requests[0].messages
    );

    // The host face keeps the upstream pair: `message_start` announces the
    // original, `message_end` carries the finalized (replaced) message.
    let entries = log.lock().unwrap();
    let types = entries
        .iter()
        .map(|(kind, _)| kind.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        &types[..4],
        ["agent_start", "turn_start", "message_start", "message_end"],
        "{types:?}"
    );
    assert_eq!(
        entries[2].1["message"]["content"][0]["text"],
        serde_json::json!("original")
    );
    assert_eq!(
        entries[3].1["message"]["content"][0]["text"],
        serde_json::json!("replaced")
    );

    // The durable transcript stores the replacement, not the original.
    let reopened = SessionManager::open(&path).expect("reopen session");
    let messages = reopened.build_context_entries().expect("context entries");
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[0].role(), "user");
    assert_eq!(text_of_message(&messages[0]).as_deref(), Some("replaced"));
    assert_eq!(text_of_message(&messages[1]).as_deref(), Some("ok"));
    let raw = std::fs::read_to_string(&path).expect("read session file");
    assert!(
        !raw.contains("\"original\""),
        "the replaced prompt must never reach storage: {raw}"
    );
}

// ---------------------------------------------------------------------------
// Lane D: one projection per compaction-free prompt; paged widen off-thread
// ---------------------------------------------------------------------------

/// D4: the prompt-start compaction gate must not build the transcript twice
/// when nothing compacts. With compaction ENABLED but a tiny transcript (no
/// threshold trigger), the gate's projection is reused as the run context.
#[tokio::test]
async fn prompt_builds_one_projection_when_no_compaction_runs() {
    let temp = TempDir::new("one-projection");
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(None),
        session: Some(session),
        // Enabled: the pre-prompt gate runs, but the threshold is far away.
        compaction: Some(pir::compaction::CompactionConfig::default()),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    agent.prompt("hi", Arc::new(|_| {})).await.expect("prompt");

    let builds = agent
        .with_session_manager(|manager| manager.projection_build_count())
        .await;
    assert_eq!(
        builds,
        Some(1),
        "a compaction-free prompt must build the projection exactly once \
         (the gate's projection is reused as the run context)"
    );
}

/// D6: a PAGED session's run-start widen happens on the blocking pool, not on
/// the async runtime worker (the stall seam runs on the widening thread).
#[tokio::test]
async fn paged_run_widens_off_the_runtime_thread() {
    let temp = TempDir::new("paged-widen-off-thread");
    let mut storage = SessionManager::create(".", &temp.0).expect("create session");
    for index in 0..40 {
        storage
            .append_message(Message::User(UserMessage {
                role: UserRole::User,
                content: vec![text(&format!("user {index} {}", "x".repeat(512)))],
                tools_added: None,
                tools_removed: None,
                timestamp: 1,
                sections: None,
            }))
            .expect("append user message");
        storage
            .append_message(Message::Assistant(AssistantMessage {
                content: vec![text(&format!("assistant {index}"))],
                stop_reason: StopReason::Stop,
                timestamp: 1,
                ..Default::default()
            }))
            .expect("append assistant message");
    }
    let path = storage.path().to_path_buf();
    drop(storage);
    let mut paged = SessionManager::open_paged(&path, 1024, 4).expect("paged open");
    assert!(paged.is_paged(), "the fixture must exceed the page caps");
    paged.stall_next_widen_for_test(std::time::Duration::from_millis(800));

    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(None),
        session: Some(paged),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    let prompt = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move { agent.prompt("hi", Arc::new(|_| {})).await }
    });
    // Current-thread runtime: a blocking widen would prevent this timer from
    // firing until the stall ended.
    let probe = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        tokio::time::sleep(std::time::Duration::from_millis(40)),
    )
    .await;
    assert!(
        probe.is_ok(),
        "the runtime must stay responsive while the paged widen is stalled"
    );
    assert!(
        !prompt.is_finished(),
        "the run should still be inside the stalled widen"
    );

    prompt.await.expect("prompt task join").expect("prompt");
    let still_paged = agent
        .with_session_manager(|manager| manager.is_paged())
        .await;
    assert_eq!(
        still_paged,
        Some(false),
        "the run must widen the paged session to the full transcript"
    );
}

/// A tool that terminates the low-level loop run, leaving the context on a
/// tool result — the shape an `agent_before_settle` continuation needs
/// (`context_can_continue`).
struct TerminatingTool;

impl AgentTool for TerminatingTool {
    fn name(&self) -> &'static str {
        "stop"
    }

    fn description(&self) -> &'static str {
        "stop the loop run"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "type": "object" }))
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async {
            let mut result = ToolResult::ok("stop now");
            result.terminate = true;
            result
        })
    }
}

/// `agent_before_settle` handler that parks the settle boundary and then asks
/// for exactly one continuation. The first call waits on the gate and returns
/// `{continue: true}`; later calls settle normally.
struct SettleContinuationGate {
    info: PluginInfo,
    entered: Arc<AtomicUsize>,
    gate: tokio::sync::watch::Receiver<bool>,
}

impl AgentPlugin for SettleContinuationGate {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::List(vec!["agent_before_settle".into()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        if event.name != "agent_before_settle" {
            return Box::pin(async { PluginEventOutcome::None });
        }
        let first = self.entered.fetch_add(1, Ordering::SeqCst) == 0;
        let mut gate = self.gate.clone();
        Box::pin(async move {
            if !first {
                return PluginEventOutcome::None;
            }
            // Bounded wait: a never-released gate must fail the test instead of
            // hanging the suite.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if *gate.borrow() {
                        break;
                    }
                    if gate.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
            PluginEventOutcome::Generic(serde_json::json!({ "continue": true }))
        })
    }
}

/// FIX-A08 (R1-L1-007): a projection read failure on a settle CONTINUATION
/// must abort the run instead of silently sending an empty context. A paged
/// session is swapped in while the first run is parked at the
/// `agent_before_settle` boundary; its file is then replaced by a directory so
/// the continuation's `widen_to_full` fails. The prompt must return `Err`
/// with no second provider request.
#[tokio::test]
async fn settle_continuation_projection_failure_aborts_the_run() {
    let temp = TempDir::new("settle-projection-failure");

    // A paged fixture with a skipped prefix. The path stays valid until the
    // test swaps it in; `open_paged` does not hold the file mapping.
    let mut full = SessionManager::create(".", &temp.0).expect("create paged fixture");
    for index in 0..40 {
        full.append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text(&format!("paged user {index} {}", "x".repeat(512)))],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("append paged user");
        full.append_message(Message::Assistant(assistant(
            vec![text(&format!("paged reply {index}"))],
            StopReason::Stop,
        )))
        .expect("append paged assistant");
    }
    let paged_path = full.path().to_path_buf();
    drop(full);
    let paged = SessionManager::open_paged(&paged_path, 1024, 4).expect("paged open");
    assert!(paged.is_paged(), "the fixture must exceed the page caps");

    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_responder = Arc::clone(&requests);
    let registry = fake_registry(Arc::new(move |_index, _context| {
        if requests_for_responder.fetch_add(1, Ordering::SeqCst) == 0 {
            assistant(vec![tool_call("stop-1", "stop")], StopReason::ToolUse)
        } else {
            assistant(vec![text("must not run")], StopReason::Stop)
        }
    }));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(TerminatingTool));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let mut config = race_agent_config(registry, session, &temp, None);
    config.tools = tools;
    let agent = Arc::new(AgentSession::new_with_plugins(
        config,
        vec![Arc::new(SettleContinuationGate {
            info: PluginInfo::inline("settle-continuation-gate"),
            entered: Arc::clone(&entered),
            gate: gate_rx,
        })],
    ));

    let prompt = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("start", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_counter(&entered, 1, "agent_before_settle gate").await;

    // Install the paged store, then make its backing path unreadable: the
    // continuation's projection must fail, not fall back to an empty context.
    assert!(
        agent.replace_session_storage(paged).await,
        "the paged store must be installed"
    );
    std::fs::remove_file(&paged_path).expect("remove paged session file");
    std::fs::create_dir(&paged_path).expect("replace paged session file with a directory");

    let _ = gate_tx.send(true);
    let result = prompt.await.expect("prompt task");
    assert!(
        result.is_err(),
        "a settle-continuation projection failure must abort the run"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "no second provider request may run with an empty projected context"
    );
}

/// Declares `before_agent_start` so the prompt run drains the notifications a
/// handler queued at the handler boundary (the run only drains when some
/// plugin claims that event).
struct BeforeAgentStartInterest {
    info: PluginInfo,
}

impl BeforeAgentStartInterest {
    fn new() -> Self {
        Self {
            info: PluginInfo::inline("before-agent-start-interest"),
        }
    }
}

impl AgentPlugin for BeforeAgentStartInterest {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::List(vec!["before_agent_start".to_owned()])
    }
}

/// D4: the prompt-start projection snapshot must not be reused after a
/// `before_agent_start` handler appended a session entry. A handler-time
/// `pi.sendMessage({triggerTurn:false})` persists a model-visible custom
/// message AFTER the gate's projection was built; reusing that projection
/// drops the message from the run's first request.
#[tokio::test]
async fn before_agent_start_append_invalidates_the_reused_projection() {
    let temp = TempDir::new("d4-handler-append");
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_responder = Arc::clone(&requests);
    let registry = fake_registry(Arc::new(move |_index, context| {
        requests_for_responder
            .lock()
            .unwrap()
            .push(context.messages.clone());
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    // Plan-only handle: the JS-facing notification queue without QuickJS.
    let handle = ExtensionHandle::start_plan_only(ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    });
    // A large window keeps the threshold hook idle: the gate must BUILD and
    // then REUSE its projection on prompt 2 (a compaction would rebuild it
    // anyway and hide the stale-reuse defect). The default 16k reserve is
    // above `fake_model`'s 4k window, which would compact every non-empty
    // transcript of the fixture.
    let mut model = fake_model();
    model.context_window = 1_000_000;
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model,
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: Some(session),
            // Enabled: the prompt-start gate builds and (when nothing
            // compacts) reuses a projection.
            compaction: Some(pir::compaction::CompactionConfig::default()),
            max_turns: 4,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: Some(handle.clone()),
            tool_selection: None,
        },
        vec![Arc::new(BeforeAgentStartInterest::new())],
    ));

    // Prompt 1 marks `session_start_sent` and leaves an assistant tail — the
    // gate's snapshot condition for prompt 2.
    agent
        .prompt("first", event_handler(EventLog::default()))
        .await
        .expect("first prompt");

    // The handler boundary drains this queue; `triggerTurn: false` appends the
    // custom message to the session instead of steering the run.
    handle.push_notification(Notification::SendMessage {
        payload: serde_json::json!({
            "message": {
                "customType": "handler-note",
                "content": "persisted at the handler boundary",
                "display": false,
            },
            "options": { "triggerTurn": false },
        }),
    });
    agent
        .prompt("second", event_handler(EventLog::default()))
        .await
        .expect("second prompt");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "one provider request per prompt");
    let second = requests.last().expect("second request");
    assert!(
        second.iter().any(|message| matches!(
            message,
            Message::User(user) if user.content.iter().any(|block| matches!(
                block,
                ContentBlock::Text(text) if text.text == "persisted at the handler boundary"
            ))
        )),
        "the handler-appended custom message must reach this run's first \
         request instead of being hidden by the reused projection: {second:?}"
    );
}

// ---------------------------------------------------------------------------
// R1-L1-002: an in-flight prompt must not persist into a swapped-in session
// ---------------------------------------------------------------------------

/// Provider whose response stays in flight until the test releases it: the
/// run is genuinely streaming (no assistant message produced yet) when the
/// storage swap lands, so the per-message sink is the live write path the
/// swap must isolate. The first response asks for the always-ready `gate`
/// tool so the post-swap tail also covers a persisted tool result.
struct HangingProvider {
    started: Arc<AtomicUsize>,
    release: tokio::sync::watch::Receiver<bool>,
    calls: AtomicUsize,
}

impl Provider for HangingProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
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
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        let producer = stream.producer();
        let mut release = self.release.clone();
        let message = if call == 0 {
            assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
        } else {
            assistant(vec![text("old run answer")], StopReason::Stop)
        };
        tokio::spawn(async move {
            while !*release.borrow() {
                if release.changed().await.is_err() {
                    return;
                }
            }
            producer.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
        });
        stream
    }
}

async fn wait_for_provider_start(started: &Arc<AtomicUsize>) {
    for _ in 0..2000 {
        if started.load(Ordering::SeqCst) > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("provider request never started");
}

/// Message roles persisted in one JSONL session file, in entry order.
fn persisted_message_roles(path: &std::path::Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).expect("session file");
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["type"] == "message")
        .filter_map(|entry| entry["message"]["role"].as_str().map(str::to_owned))
        .collect()
}

/// Number of compaction entries persisted in one JSONL session file.
fn persisted_compaction_entries(path: &std::path::Path) -> usize {
    let text = std::fs::read_to_string(path).expect("session file");
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["type"] == "compaction")
        .count()
}

/// Assert a session file holds no compaction entry. The outgoing transcript
/// must drop the old cut (its manager no longer owns the slot); the
/// replacement must never receive a `firstKeptEntryId` that only exists in
/// another file.
fn assert_no_compaction_entry(path: &std::path::Path, what: &str) {
    assert_eq!(
        persisted_compaction_entries(path),
        0,
        "{what} must not receive a compaction entry"
    );
}

fn seed_user_message(text_value: &str, timestamp: u64) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text(text_value)],
        tools_added: None,
        tools_removed: None,
        timestamp,
        sections: None,
    })
}

/// R1-L1-002: a prompt that is still waiting on its provider when
/// `replace_session_storage` installs a new manager must not append its
/// assistant/tool-result `message_end` writes to the replacement transcript.
/// On the old sink (which held the stable session SLOT, not the concrete
/// manager identity) the late assistant landed in the new session — an
/// assistant without its user prompt.
#[tokio::test]
async fn in_flight_prompt_does_not_persist_into_replaced_session_storage() {
    let temp = TempDir::new("swap-in-flight-prompt");
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let started = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(HangingProvider {
        started: Arc::clone(&started),
        release: release_rx,
        calls: AtomicUsize::new(0),
    }));
    // The gate is already open: the post-swap tool batch runs immediately, so
    // its tool-result `message_end` is a post-swap sink write too.
    let (_gate_tx, gate_rx) = tokio::sync::watch::channel(true);
    let entered = Arc::new(AtomicUsize::new(0));

    let mut first = SessionManager::create(".", &temp.0).expect("create session");
    first
        .append_message(seed_user_message("old seed", 1))
        .expect("seed outgoing");
    let first_path = first.path().to_path_buf();

    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: tool_registry_with(Some((gate_rx, Arc::clone(&entered)))),
        session: Some(first),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }));

    // The prompt run is parked INSIDE its first provider request: the sink
    // already exists and no assistant message has been produced yet.
    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("old prompt", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_provider_start(&started).await;

    // Swap the storage while the run is in flight (the reported bug window).
    let mut second = SessionManager::create(".", &temp.0).expect("create replacement");
    second
        .append_message(seed_user_message("new seed", 2))
        .expect("seed replacement");
    let second_path = second.path().to_path_buf();
    assert!(
        agent.replace_session_storage(second).await,
        "the swap must succeed"
    );
    assert!(
        persisted_message_roles(&second_path) == vec!["user".to_owned()],
        "the outgoing run must not have leaked before its response arrived"
    );

    // Let the old run complete against its now-detached transcript.
    let _ = release_tx.send(true);
    let answer = prompt_task
        .await
        .expect("prompt task")
        .expect("prompt result");
    assert_eq!(answer.stop_reason, StopReason::Stop);
    assert!(
        entered.load(Ordering::SeqCst) > 0,
        "the post-swap tool batch never ran; the tool-result assertion would be vacuous"
    );

    // The replacement session holds ONLY its own seed; neither the old
    // run's assistant nor a tool result may appear in it.
    let replacement_roles = persisted_message_roles(&second_path);
    assert_eq!(
        replacement_roles,
        vec!["user".to_owned()],
        "the in-flight old run persisted into the replacement session"
    );
    let replacement = std::fs::read_to_string(&second_path).expect("replacement file");
    assert!(
        !replacement.contains("gate open"),
        "the old run's tool result leaked into the replacement session: {replacement}"
    );
    assert!(
        !replacement.contains("old run answer"),
        "the old run's assistant leaked into the replacement session: {replacement}"
    );
    let replacement_entries = agent
        .with_session_manager(|manager| manager.entries())
        .await
        .expect("replacement manager");
    assert!(
        replacement_entries.iter().all(|entry| match entry {
            pir::session_manager::FileEntry::Message(message) => matches!(
                &message.message,
                pir::session_manager::types::StoredMessage::User(_)
            ),
            _ => true,
        }),
        "the replacement manager gained a non-user entry from the old run"
    );

    // The outgoing transcript keeps what it persisted before the swap (its
    // seed, the starting user message, and the run's tool declaration); by
    // design the post-swap assistant/tool result are discarded and
    // debug-logged rather than redirected anywhere.
    let outgoing_roles = persisted_message_roles(&first_path);
    assert_eq!(
        outgoing_roles
            .iter()
            .filter(|role| role.as_str() == "user")
            .count(),
        2,
        "the outgoing transcript must keep its own pre-swap user entries: {outgoing_roles:?}"
    );
    assert!(
        !outgoing_roles.iter().any(|role| role == "assistant"),
        "a post-swap assistant must not be redirected into the outgoing session: {outgoing_roles:?}"
    );
    assert!(
        !outgoing_roles.iter().any(|role| role == "toolResult"),
        "a post-swap tool result must not be redirected into the outgoing session: {outgoing_roles:?}"
    );
    let outgoing = std::fs::read_to_string(&first_path).expect("outgoing file");
    assert!(
        outgoing.contains("old prompt"),
        "the starting user message belongs to the outgoing transcript: {outgoing}"
    );
    assert!(
        !outgoing.contains("old run answer"),
        "a post-swap assistant must not be redirected into the outgoing session: {outgoing}"
    );
}

// ---------------------------------------------------------------------------
// R1-L1-002 P1-01 / P2-01 / P2-03: run-start, post-run batch and compaction
// writes must all stay on the transcript the run bound at its start
// ---------------------------------------------------------------------------

/// Provider whose single response is a final assistant that stays in flight
/// until the test releases it. The prompt run is genuinely streaming (the
/// starting message is already persisted, no assistant exists yet) when the
/// test arms an append failure or swaps storage.
struct HangingFinalProvider {
    started: Arc<AtomicUsize>,
    calls: AtomicUsize,
    release: tokio::sync::watch::Receiver<bool>,
}

impl Provider for HangingFinalProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
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
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        let producer = stream.producer();
        let mut release = self.release.clone();
        tokio::spawn(async move {
            while !*release.borrow() {
                if release.changed().await.is_err() {
                    return;
                }
            }
            producer.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant(vec![text("old run answer")], StopReason::Stop),
            ));
        });
        stream
    }
}

/// P1-01 regression: the run binds its session identity BEFORE the
/// `before_agent_start` dispatch, so a swap landing there cannot let the
/// starting prompt (the unguarded `preamble_persist_fn` slot write in the old
/// implementation) enter the replacement transcript. The run fails before its
/// first provider request instead of answering into a session it must not
/// write.
#[tokio::test]
async fn run_start_swap_does_not_persist_into_replaced_session_storage() {
    let temp = TempDir::new("swap-run-start");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let calls_for_responder = Arc::clone(&provider_calls);
    let registry = fake_registry(Arc::new(move |_index, _context| {
        calls_for_responder.fetch_add(1, Ordering::SeqCst);
        assistant(vec![text("old run answer")], StopReason::Stop)
    }));

    let mut first = SessionManager::create(".", &temp.0).expect("create session");
    first
        .append_message(seed_user_message("old seed", 1))
        .expect("seed outgoing");
    let first_path = first.path().to_path_buf();
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: Some(first),
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![Arc::new(GatePlugin {
            info: PluginInfo::inline("before-agent-start-gate"),
            event: "before_agent_start",
            entered: Arc::clone(&entered),
            gate: gate_rx,
        })],
    ));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("old prompt", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_counter(&entered, 1, "before_agent_start gate").await;

    // The swap lands between run-start identity capture and the preamble
    // write / first provider request: exactly the P1-01 window.
    let mut second = SessionManager::create(".", &temp.0).expect("create replacement");
    second
        .append_message(seed_user_message("new seed", 2))
        .expect("seed replacement");
    let second_id = second.id().to_owned();
    let second_path = second.path().to_path_buf();
    assert!(
        agent.replace_session_storage(second).await,
        "the swap must succeed"
    );
    // P2-02: the cached identity is refreshed in the swap critical section,
    // not after the (transcript-sized) tree build.
    assert_eq!(
        agent.session_id().as_deref(),
        Some(second_id.as_str()),
        "the cached session identity must name the replacement immediately after the swap"
    );

    let _ = gate_tx.send(true);
    let result = prompt_task.await.expect("prompt task");
    assert!(
        result.is_err(),
        "a run whose starting message cannot be persisted must fail before its first request"
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        0,
        "the aborted run must not issue a provider request"
    );

    let replacement_roles = persisted_message_roles(&second_path);
    assert_eq!(
        replacement_roles,
        vec!["user".to_owned()],
        "the run-start swap leaked the old prompt/answer into the replacement: {replacement_roles:?}"
    );
    let replacement = std::fs::read_to_string(&second_path).expect("replacement file");
    assert!(
        !replacement.contains("old prompt"),
        "the old run's starting prompt entered the replacement: {replacement}"
    );
    assert!(
        !replacement.contains("old run answer"),
        "the old run's assistant entered the replacement: {replacement}"
    );

    // The outgoing transcript keeps what it persisted before the swap (its
    // seed); the starting prompt was never written anywhere because the run
    // aborted at the guard.
    let outgoing_roles = persisted_message_roles(&first_path);
    assert_eq!(
        outgoing_roles,
        vec!["user".to_owned()],
        "the outgoing transcript must keep its own pre-swap entries only: {outgoing_roles:?}"
    );
}

/// P2-01 regression: when the per-message sink fails, the ledger leaves the
/// ordinal (and the tail) to the post-run batch. A storage swap landing
/// between the failure and the batch must not let the batch replay the old
/// run's assistant into the replacement session.
#[tokio::test]
async fn post_run_batch_does_not_replay_into_replaced_session_storage() {
    let temp = TempDir::new("swap-post-run-batch");
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let started = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(HangingFinalProvider {
        started: Arc::clone(&started),
        calls: AtomicUsize::new(0),
        release: release_rx,
    }));
    let (turn_tx, turn_rx) = tokio::sync::watch::channel(false);
    let turn_entered = Arc::new(AtomicUsize::new(0));

    let mut first = SessionManager::create(".", &temp.0).expect("create session");
    first
        .append_message(seed_user_message("old seed", 1))
        .expect("seed outgoing");
    let first_path = first.path().to_path_buf();
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: Arc::new(registry),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: Some(first),
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![Arc::new(GatePlugin {
            info: PluginInfo::inline("turn-end-gate"),
            event: "turn_end",
            entered: Arc::clone(&turn_entered),
            gate: turn_rx,
        })],
    ));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("old prompt", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_provider_start(&started).await;

    // The starting message is persisted; make the terminal assistant's
    // per-message sink write fail so the ledger installs its barrier and the
    // post-run batch owns that message.
    agent
        .with_session_manager(|manager| {
            manager.fail_next_append_for_test(pir::session_manager::AppendFailure::BeforeWrite)
        })
        .await
        .expect("session manager");
    let _ = release_tx.send(true);
    wait_for_counter(&turn_entered, 1, "turn_end gate").await;

    // Swap AFTER the failed sink write and BEFORE the post-run batch.
    let mut second = SessionManager::create(".", &temp.0).expect("create replacement");
    second
        .append_message(seed_user_message("new seed", 2))
        .expect("seed replacement");
    let second_path = second.path().to_path_buf();
    assert!(
        agent.replace_session_storage(second).await,
        "the swap must succeed"
    );
    let _ = turn_tx.send(true);

    let answer = prompt_task
        .await
        .expect("prompt task")
        .expect("prompt result");
    assert_eq!(
        text_of_message(&Message::Assistant(answer)).as_deref(),
        Some("old run answer")
    );

    let replacement_roles = persisted_message_roles(&second_path);
    assert_eq!(
        replacement_roles,
        vec!["user".to_owned()],
        "the post-run batch replayed the old run's tail into the replacement: {replacement_roles:?}"
    );
    let replacement = std::fs::read_to_string(&second_path).expect("replacement file");
    assert!(
        !replacement.contains("old run answer"),
        "the post-run batch replayed the old assistant into the replacement: {replacement}"
    );

    let outgoing_roles = persisted_message_roles(&first_path);
    assert_eq!(
        outgoing_roles
            .iter()
            .filter(|role| role.as_str() == "user")
            .count(),
        2,
        "the outgoing transcript must keep its seed + starting prompt: {outgoing_roles:?}"
    );
    assert!(
        !outgoing_roles.iter().any(|role| role == "assistant"),
        "the failed sink append must not be redirected into the outgoing session: {outgoing_roles:?}"
    );
}

/// Summarizer provider for the overflow-recovery compaction test: the main
/// model call terminates with a context-overflow error, and the summarization
/// call stays in flight until the test releases it (the swap window).
struct HangingSummarizerProvider {
    summarizer_started: Arc<AtomicUsize>,
    summarizer_release: tokio::sync::watch::Receiver<bool>,
}

impl Provider for HangingSummarizerProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
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
        let stream = AssistantMessageEventStream::new();
        let producer = stream.producer();
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            self.summarizer_started.fetch_add(1, Ordering::SeqCst);
            let mut release = self.summarizer_release.clone();
            tokio::spawn(async move {
                while !*release.borrow() {
                    if release.changed().await.is_err() {
                        return;
                    }
                }
                producer.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    assistant(vec![text("compacted summary")], StopReason::Stop),
                ));
            });
        } else {
            let mut message = assistant(Vec::new(), StopReason::Error);
            message.error_message =
                Some("prompt is too long: 200000 tokens > 16384 maximum".into());
            producer.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
        }
        stream
    }
}

/// P2-03 regression: the prompt-internal overflow-recovery compaction is
/// generated while the run is parked in the summarizer request. A swap in
/// that window must not append the outgoing transcript's compaction (with a
/// `firstKeptEntryId` that only exists in the old file) to the replacement
/// session.
#[tokio::test]
async fn overflow_compaction_does_not_write_into_replaced_session_storage() {
    let temp = TempDir::new("swap-overflow-compaction");
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let summarizer_started = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(HangingSummarizerProvider {
        summarizer_started: Arc::clone(&summarizer_started),
        summarizer_release: release_rx,
    }));

    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    let first_path = session.path().to_path_buf();
    let agent = Arc::new(AgentSession::new(race_agent_config(
        Arc::new(registry),
        session,
        &temp,
        None,
    )));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("trigger overflow", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_counter(&summarizer_started, 1, "overflow summarizer request").await;

    // Swap while the summarizer is in flight: the upcoming compaction append
    // must target the outgoing transcript only.
    let mut second = SessionManager::create(".", &temp.0).expect("create replacement");
    second
        .append_message(seed_user_message("new seed", 2))
        .expect("seed replacement");
    let second_path = second.path().to_path_buf();
    assert!(
        agent.replace_session_storage(second).await,
        "the swap must succeed"
    );
    let _ = release_tx.send(true);

    let terminal = prompt_task
        .await
        .expect("prompt task")
        .expect("the overflow terminal still settles the prompt");
    assert_eq!(
        terminal.stop_reason,
        StopReason::Error,
        "the recovery compaction was dropped, so the run settles on its overflow terminal"
    );

    let replacement_roles = persisted_message_roles(&second_path);
    assert_eq!(
        replacement_roles,
        vec!["user".to_owned()],
        "the recovery compaction/messages leaked into the replacement: {replacement_roles:?}"
    );
    let replacement = std::fs::read_to_string(&second_path).expect("replacement file");
    assert!(
        !replacement.contains("compacted summary"),
        "the old run's compaction summary entered the replacement: {replacement}"
    );

    // The outgoing transcript keeps the raw failed attempt (persisted as a
    // pair by design) but receives no recovery compaction after the swap.
    let outgoing = std::fs::read_to_string(&first_path).expect("outgoing file");
    assert!(
        outgoing.contains("prompt is too long"),
        "the outgoing transcript must keep its raw overflow attempt: {outgoing}"
    );
    let reopened = SessionManager::open(&first_path).expect("reopen outgoing session");
    let branch = reopened.branch_entries(None).expect("branch entries");
    assert!(
        !branch
            .iter()
            .any(|entry| matches!(entry, pir::session_manager::FileEntry::Compaction(_))),
        "the outgoing transcript must not receive a compaction entry from the aborted recovery"
    );
}

// ---------------------------------------------------------------------------
// P2-03 (lane SESSION3): manual / threshold / extension-replacement
// compaction appends must be bound to the manager the compaction started on
// ---------------------------------------------------------------------------

/// Plugin for the extension-replacement regression: parks in
/// `session_before_compact` (the test swaps storage while it waits) and then
/// returns an extension-provided `compaction` replacement whose
/// `firstKeptEntryId` names an entry of the OUTGOING transcript.
struct ReplacementCompactionPlugin {
    info: PluginInfo,
    entered: Arc<AtomicUsize>,
    gate: tokio::sync::watch::Receiver<bool>,
    summary: &'static str,
    first_kept_entry_id: &'static str,
}

impl AgentPlugin for ReplacementCompactionPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::List(vec!["session_before_compact".into()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        if event.name != "session_before_compact" {
            return Box::pin(async { PluginEventOutcome::None });
        }
        let entered = Arc::clone(&self.entered);
        let mut gate = self.gate.clone();
        let summary = self.summary;
        let first_kept_entry_id = self.first_kept_entry_id;
        Box::pin(async move {
            entered.fetch_add(1, Ordering::SeqCst);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if *gate.borrow() {
                        break;
                    }
                    if gate.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
            PluginEventOutcome::Generic(serde_json::json!({
                "compaction": {
                    "summary": summary,
                    "firstKeptEntryId": first_kept_entry_id,
                    "tokensBefore": 1234,
                }
            }))
        })
    }
}

/// P2-03 regression (MANUAL path): `/compact` parks in
/// `session_before_compact`; a storage swap before the summarizer returns
/// must not append the outgoing transcript's compaction (whose
/// `firstKeptEntryId` only exists in the old file) to the replacement
/// session. The manual compaction reports the failure instead of a summary
/// that was never persisted.
#[tokio::test]
async fn manual_compaction_swap_does_not_write_into_replaced_session_storage() {
    let temp = TempDir::new("swap-manual-compaction");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("MANUAL SUMMARY")], StopReason::Stop);
        }
        assistant(vec![text("late")], StopReason::Stop)
    }));

    let mut first = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut first);
    let first_path = first.path().to_path_buf();
    let agent = Arc::new(AgentSession::new_with_plugins(
        race_agent_config(registry, first, &temp, None),
        vec![Arc::new(GatePlugin {
            info: PluginInfo::inline("manual-compact-gate"),
            event: "session_before_compact",
            entered: Arc::clone(&entered),
            gate: gate_rx,
        })],
    ));

    let compact_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move { agent.compact(None).await }
    });
    wait_for_counter(&entered, 1, "session_before_compact gate").await;

    // Swap while the compaction is parked before its summarizer request:
    // the upcoming append must target the outgoing transcript only.
    let mut second = SessionManager::create(".", &temp.0).expect("create replacement");
    second
        .append_message(seed_user_message("new seed", 2))
        .expect("seed replacement");
    let second_path = second.path().to_path_buf();
    assert!(
        agent.replace_session_storage(second).await,
        "the swap must succeed"
    );
    let _ = gate_tx.send(true);

    let error = compact_task
        .await
        .expect("compact task")
        .expect_err("a compaction whose storage was swapped must fail");
    assert!(
        error.to_string().contains("session storage was replaced"),
        "unexpected compaction error: {error}"
    );
    assert!(
        !agent.is_compacting(),
        "the failed compaction must release its claim"
    );

    assert_no_compaction_entry(&second_path, "the replacement");
    let replacement = std::fs::read_to_string(&second_path).expect("replacement file");
    assert!(
        !replacement.contains("MANUAL SUMMARY"),
        "the old compaction summary entered the replacement: {replacement}"
    );
    let replacement_roles = persisted_message_roles(&second_path);
    assert_eq!(
        replacement_roles,
        vec!["user".to_owned()],
        "the replacement must keep only its own seed: {replacement_roles:?}"
    );

    // The outgoing transcript keeps its seeded history; the compaction ran
    // after the swap, so its append is dropped by design.
    assert_no_compaction_entry(&first_path, "the outgoing transcript");
    let outgoing_roles = persisted_message_roles(&first_path);
    assert_eq!(
        outgoing_roles,
        vec![
            "user".to_owned(),
            "assistant".to_owned(),
            "user".to_owned(),
            "assistant".to_owned()
        ],
        "the outgoing transcript must keep its seeded history only: {outgoing_roles:?}"
    );
}

/// P2-03 regression (THRESHOLD normal path): the prompt-start threshold gate
/// parks in `session_before_compact`; a storage swap during the summarizer
/// must drop the old transcript's compaction instead of writing its cut into
/// the replacement session.
#[tokio::test]
async fn threshold_compaction_swap_does_not_write_into_replaced_session_storage() {
    let temp = TempDir::new("swap-threshold-compaction");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("THRESHOLD SUMMARY")], StopReason::Stop);
        }
        assistant(vec![text("model reply")], StopReason::Stop)
    }));

    let mut first = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut first);
    let first_path = first.path().to_path_buf();
    let mut model = fake_model();
    // Tiny window: the seeded history is over the threshold at prompt start.
    model.context_window = 64;
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model,
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: tool_registry_with(None),
            session: Some(first),
            compaction: Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            }),
            max_turns: 4,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![Arc::new(GatePlugin {
            info: PluginInfo::inline("threshold-compact-gate"),
            event: "session_before_compact",
            entered: Arc::clone(&entered),
            gate: gate_rx,
        })],
    ));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("trigger threshold", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_counter(&entered, 1, "threshold session_before_compact gate").await;

    let mut second = SessionManager::create(".", &temp.0).expect("create replacement");
    second
        .append_message(seed_user_message("new seed", 2))
        .expect("seed replacement");
    let second_path = second.path().to_path_buf();
    assert!(
        agent.replace_session_storage(second).await,
        "the swap must succeed"
    );
    let _ = gate_tx.send(true);

    // The threshold compaction is dropped; the run then fails when its
    // starting message cannot be persisted into the replacement either.
    let error = prompt_task
        .await
        .expect("prompt task")
        .expect_err("the run must abort after its storage was replaced");
    assert!(
        error.to_string().contains("session storage was replaced"),
        "unexpected prompt error: {error}"
    );
    assert!(
        !agent.is_compacting(),
        "the failed compaction must release its claim"
    );

    assert_no_compaction_entry(&second_path, "the replacement");
    let replacement = std::fs::read_to_string(&second_path).expect("replacement file");
    assert!(
        !replacement.contains("THRESHOLD SUMMARY"),
        "the old threshold compaction entered the replacement: {replacement}"
    );
    let replacement_roles = persisted_message_roles(&second_path);
    assert_eq!(
        replacement_roles,
        vec!["user".to_owned()],
        "the replacement must keep only its own seed: {replacement_roles:?}"
    );

    assert_no_compaction_entry(&first_path, "the outgoing transcript");
    let outgoing = std::fs::read_to_string(&first_path).expect("outgoing file");
    assert!(
        !outgoing.contains("THRESHOLD SUMMARY"),
        "the outgoing transcript must not receive the dropped compaction: {outgoing}"
    );
}

/// P2-03 regression (THRESHOLD extension-replacement path): a
/// `session_before_compact` handler replaces the compaction with its own
/// result while the test swaps storage. The replacement's
/// `firstKeptEntryId` belongs to the outgoing transcript and must not be
/// appended to the replacement session.
#[tokio::test]
async fn extension_compaction_swap_does_not_write_into_replaced_session_storage() {
    let temp = TempDir::new("swap-extension-compaction");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("model reply")], StopReason::Stop)
    }));

    let mut first = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut first);
    let first_path = first.path().to_path_buf();
    let mut model = fake_model();
    model.context_window = 64;
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model,
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: tool_registry_with(None),
            session: Some(first),
            compaction: Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            }),
            max_turns: 4,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![Arc::new(ReplacementCompactionPlugin {
            info: PluginInfo::inline("extension-compaction-replacement"),
            entered: Arc::clone(&entered),
            gate: gate_rx,
            summary: "EXTENSION SUMMARY",
            first_kept_entry_id: "entry-only-in-the-outgoing-transcript",
        })],
    ));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt(
                    "trigger extension threshold",
                    event_handler(EventLog::default()),
                )
                .await
        }
    });
    wait_for_counter(&entered, 1, "extension session_before_compact gate").await;

    let mut second = SessionManager::create(".", &temp.0).expect("create replacement");
    second
        .append_message(seed_user_message("new seed", 2))
        .expect("seed replacement");
    let second_path = second.path().to_path_buf();
    assert!(
        agent.replace_session_storage(second).await,
        "the swap must succeed"
    );
    let _ = gate_tx.send(true);

    let error = prompt_task
        .await
        .expect("prompt task")
        .expect_err("the run must abort after its storage was replaced");
    assert!(
        error.to_string().contains("session storage was replaced"),
        "unexpected prompt error: {error}"
    );
    assert!(
        !agent.is_compacting(),
        "the failed compaction must release its claim"
    );

    assert_no_compaction_entry(&second_path, "the replacement");
    let replacement = std::fs::read_to_string(&second_path).expect("replacement file");
    assert!(
        !replacement.contains("EXTENSION SUMMARY"),
        "the extension compaction entered the replacement: {replacement}"
    );
    assert!(
        !replacement.contains("entry-only-in-the-outgoing-transcript"),
        "the dangling firstKeptEntryId entered the replacement: {replacement}"
    );

    assert_no_compaction_entry(&first_path, "the outgoing transcript");
    let outgoing = std::fs::read_to_string(&first_path).expect("outgoing file");
    assert!(
        !outgoing.contains("EXTENSION SUMMARY"),
        "the outgoing transcript must not receive the dropped compaction: {outgoing}"
    );
}

// ---------------------------------------------------------------------------
// P2-03 residual (lane SESSION4): the in-loop threshold hook must bind its
// compaction to the RUN-START transcript identity, not to the manager that
// occupies the storage slot when the threshold fires. These runs swap the
// session storage WHILE the prompt is in flight (inside a gated tool call or
// a `session_before_compact` handler), so the stale prompt-start snapshot /
// rebuilt context is later paired with the replacement manager.
// ---------------------------------------------------------------------------

/// The first model turn's answer: a long text (so the NEXT turn's threshold
/// check is far over the tiny test window) plus the `gate` tool call whose
/// run parks the prompt until the test has swapped storage.
fn gated_long_assistant() -> AssistantMessage {
    assistant(
        vec![
            text(&format!("in-loop-assistant-{}", "a".repeat(4000))),
            tool_call("call-1", "gate"),
        ],
        StopReason::ToolUse,
    )
}

/// P2-03 residual regression (in-loop threshold, FRESH prompt-start
/// snapshot): the prompt-start gate stays under the threshold, the storage
/// swap happens during the first turn's gated tool call, and the SECOND
/// turn's threshold hook must not write the outgoing transcript's compaction
/// into the replacement. On the pre-fix hook, the snapshot basis was still
/// fresh and `expectedSessionId` was read from the CURRENT slot — the
/// replacement — so the append passed and the old summary/cut landed in the
/// new session.
#[tokio::test]
async fn in_loop_threshold_swap_with_fresh_snapshot_does_not_write_into_replaced_storage() {
    let temp = TempDir::new("swap-inloop-threshold-fresh");
    let (tool_gate_tx, tool_gate_rx) = tokio::sync::watch::channel(false);
    let tool_entered = Arc::new(AtomicUsize::new(0));
    let model_turns = Arc::new(AtomicUsize::new(0));
    let registry = {
        let model_turns = Arc::clone(&model_turns);
        fake_registry(Arc::new(move |_index, context| {
            if context.system_prompt.as_deref()
                == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
            {
                return assistant(vec![text("FRESH-SNAPSHOT SUMMARY")], StopReason::Stop);
            }
            if model_turns.fetch_add(1, Ordering::SeqCst) == 0 {
                gated_long_assistant()
            } else {
                assistant(vec![text("done")], StopReason::Stop)
            }
        }))
    };

    let mut first = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut first);
    let first_path = first.path().to_path_buf();
    let mut model = fake_model();
    // Seed history ~408 tokens: BELOW the prompt-start threshold (499) and
    // below the first turn's check (~415 with the starting prompt); the
    // ~1000-token gated assistant response pushes the next turn far over.
    model.context_window = 500;
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model,
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: tool_registry_with(Some((tool_gate_rx, Arc::clone(&tool_entered)))),
            session: Some(first),
            compaction: Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            }),
            max_turns: 6,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        Vec::new(),
    ));

    let session_log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_handler(Arc::clone(&session_log)));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt(
                    "trigger the in-loop threshold",
                    event_handler(EventLog::default()),
                )
                .await
        }
    });
    wait_for_tool_entry(&tool_entered).await;

    let mut second = SessionManager::create(".", &temp.0).expect("create replacement");
    seed_compactable_history(&mut second);
    let second_path = second.path().to_path_buf();
    assert!(
        agent.replace_session_storage(second).await,
        "the swap must succeed"
    );
    let _ = tool_gate_tx.send(true);

    let terminal = prompt_task
        .await
        .expect("prompt task")
        .expect("the run continues on its old context after the swap");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal)).as_deref(),
        Some("done")
    );
    assert!(
        compaction_event_kinds(&session_log).is_empty(),
        "the swapped slot must be rejected before a compaction can start: {:?}",
        compaction_event_kinds(&session_log)
    );

    assert_no_compaction_entry(&second_path, "the replacement");
    let replacement = std::fs::read_to_string(&second_path).expect("replacement file");
    assert!(
        !replacement.contains("FRESH-SNAPSHOT SUMMARY"),
        "the stale-snapshot threshold summary entered the replacement: {replacement}"
    );
    assert_eq!(
        persisted_message_roles(&second_path),
        vec![
            "user".to_owned(),
            "assistant".to_owned(),
            "user".to_owned(),
            "assistant".to_owned()
        ],
        "the replacement must keep only its own seeded history"
    );

    // The outgoing transcript owns the run's pre-swap messages and must not
    // receive a compaction from the later (post-swap) trigger either.
    assert_no_compaction_entry(&first_path, "the outgoing transcript");
    let outgoing = std::fs::read_to_string(&first_path).expect("outgoing file");
    assert!(
        outgoing.contains("in-loop-assistant"),
        "the outgoing transcript must keep the pre-swap assistant turn: {outgoing}"
    );
    assert!(
        !outgoing.contains("FRESH-SNAPSHOT SUMMARY"),
        "the outgoing transcript must not receive the dropped compaction: {outgoing}"
    );
}

/// P2-03 residual regression (in-loop threshold, STALE REBUILT context): the
/// first turn's threshold compaction succeeds against the outgoing
/// transcript; after a gated tool call the storage is swapped and the next
/// turn's hook runs over the REBUILT context with live (compaction-stale)
/// metadata. Pre-fix, the live path captured the replacement's id together
/// with its metadata, so the old rebuilt context was summarized against the
/// replacement and appended to it. The replacement must stay untouched; the
/// outgoing transcript keeps exactly its own successful compaction.
#[tokio::test]
async fn in_loop_threshold_swap_with_stale_rebuilt_context_does_not_write_into_replaced_storage() {
    let temp = TempDir::new("swap-inloop-threshold-stale");
    let (tool_gate_tx, tool_gate_rx) = tokio::sync::watch::channel(false);
    let tool_entered = Arc::new(AtomicUsize::new(0));
    let model_turns = Arc::new(AtomicUsize::new(0));
    let registry = {
        let model_turns = Arc::clone(&model_turns);
        fake_registry(Arc::new(move |_index, context| {
            if context.system_prompt.as_deref()
                == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
            {
                return assistant(vec![text("STALE-BASE SUMMARY")], StopReason::Stop);
            }
            if model_turns.fetch_add(1, Ordering::SeqCst) == 0 {
                gated_long_assistant()
            } else {
                assistant(vec![text("done")], StopReason::Stop)
            }
        }))
    };

    let mut first = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut first);
    let first_path = first.path().to_path_buf();
    let mut model = fake_model();
    // Seed history ~408 tokens < 411: the prompt-start gate stays idle, the
    // first loop turn (seed + starting prompt ~415) compacts successfully.
    model.context_window = 412;
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model,
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: tool_registry_with(Some((tool_gate_rx, Arc::clone(&tool_entered)))),
            session: Some(first),
            compaction: Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            }),
            max_turns: 6,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        Vec::new(),
    ));

    let session_log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_handler(Arc::clone(&session_log)));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("loop threshold", event_handler(EventLog::default()))
                .await
        }
    });
    // The tool call can only run after the first turn's compaction succeeded;
    // swapping here leaves the rebuilt context paired with the replacement.
    wait_for_tool_entry(&tool_entered).await;

    let mut second = SessionManager::create(".", &temp.0).expect("create replacement");
    seed_compactable_history(&mut second);
    let second_path = second.path().to_path_buf();
    assert!(
        agent.replace_session_storage(second).await,
        "the swap must succeed"
    );
    let _ = tool_gate_tx.send(true);

    let terminal = prompt_task
        .await
        .expect("prompt task")
        .expect("the run continues on its old rebuilt context after the swap");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal)).as_deref(),
        Some("done")
    );
    assert_eq!(
        compaction_event_kinds(&session_log),
        vec!["compaction_start".to_owned(), "compaction_end".to_owned()],
        "only the pre-swap compaction may start; the post-swap rebuilt context must be refused"
    );

    assert_no_compaction_entry(&second_path, "the replacement");
    let replacement = std::fs::read_to_string(&second_path).expect("replacement file");
    assert!(
        !replacement.contains("STALE-BASE SUMMARY"),
        "the rebuilt context's summary entered the replacement: {replacement}"
    );
    assert_eq!(
        persisted_message_roles(&second_path),
        vec![
            "user".to_owned(),
            "assistant".to_owned(),
            "user".to_owned(),
            "assistant".to_owned()
        ],
        "the replacement must keep only its own seeded history"
    );

    // The outgoing transcript keeps the compaction that succeeded BEFORE the
    // swap; the post-swap trigger must not add a second one.
    assert_eq!(
        persisted_compaction_entries(&first_path),
        1,
        "the outgoing transcript must keep exactly its pre-swap compaction"
    );
    let outgoing = std::fs::read_to_string(&first_path).expect("outgoing file");
    assert!(
        outgoing.contains("STALE-BASE SUMMARY"),
        "the outgoing transcript must keep its own compaction summary: {outgoing}"
    );
}

/// P2-03 residual regression (FAILED-append retry window): the first
/// threshold attempt is parked in `session_before_compact`, the test swaps
/// storage, and the attempt's append is dropped. The next turn retries the
/// threshold with the old context. Pre-fix, the failed path left
/// `snapshot_stale` unset and the retry captured the replacement's id, so
/// the old summary/cut was written into the replacement. The retry must be
/// refused by the run-start identity, and the dropped attempt must still
/// emit the terminal end+failed event pair.
#[tokio::test]
async fn threshold_compaction_retry_after_failed_append_does_not_write_into_replaced_storage() {
    let temp = TempDir::new("swap-threshold-retry");
    let (compaction_gate_tx, compaction_gate_rx) = tokio::sync::watch::channel(false);
    let compaction_entered = Arc::new(AtomicUsize::new(0));
    // The tool gate is already open: the run only needs a second turn after
    // the failed compaction so the retry window becomes observable.
    let (_tool_gate_tx, tool_gate_rx) = tokio::sync::watch::channel(true);
    let tool_entered = Arc::new(AtomicUsize::new(0));
    let model_turns = Arc::new(AtomicUsize::new(0));
    let registry = {
        let model_turns = Arc::clone(&model_turns);
        fake_registry(Arc::new(move |_index, context| {
            if context.system_prompt.as_deref()
                == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
            {
                return assistant(vec![text("RETRY-SUMMARY")], StopReason::Stop);
            }
            if model_turns.fetch_add(1, Ordering::SeqCst) == 0 {
                gated_long_assistant()
            } else {
                assistant(vec![text("done")], StopReason::Stop)
            }
        }))
    };

    let mut first = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut first);
    let first_path = first.path().to_path_buf();
    let mut model = fake_model();
    model.context_window = 412;
    let (failed_recorder, plugin_failures) =
        PluginEventRecorder::new("threshold-failed-recorder", &["session_compact_failed"]);
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model,
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: tool_registry_with(Some((tool_gate_rx, Arc::clone(&tool_entered)))),
            session: Some(first),
            compaction: Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            }),
            max_turns: 6,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![
            Arc::new(GatePlugin {
                info: PluginInfo::inline("in-loop-threshold-retry-gate"),
                event: "session_before_compact",
                entered: Arc::clone(&compaction_entered),
                gate: compaction_gate_rx,
            }),
            Arc::new(failed_recorder),
        ],
    ));
    let session_log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_handler(Arc::clone(&session_log)));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt("loop threshold retry", event_handler(EventLog::default()))
                .await
        }
    });
    wait_for_counter(
        &compaction_entered,
        1,
        "in-loop threshold session_before_compact gate",
    )
    .await;

    // Swap while the FIRST threshold attempt is parked before its summarizer:
    // its append (old snapshot + old cut) must be dropped, and the following
    // turn must not retry the old context against the replacement.
    let mut second = SessionManager::create(".", &temp.0).expect("create replacement");
    seed_compactable_history(&mut second);
    let second_path = second.path().to_path_buf();
    assert!(
        agent.replace_session_storage(second).await,
        "the swap must succeed"
    );
    let _ = compaction_gate_tx.send(true);

    let terminal = prompt_task
        .await
        .expect("prompt task")
        .expect("the run continues on its unchanged context after the failed append");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal)).as_deref(),
        Some("done")
    );
    assert!(
        tool_entered.load(Ordering::SeqCst) > 0,
        "the run must reach the second turn (tool call) so the retry window exists"
    );

    // Exactly one terminal pair for the dropped attempt; the refused retry
    // must not start another compaction.
    assert_eq!(
        compaction_event_kinds(&session_log),
        vec!["compaction_start".to_owned(), "compaction_end".to_owned()],
        "the dropped attempt must emit exactly one start/end pair"
    );
    let end = session_log
        .lock()
        .unwrap()
        .iter()
        .find(|(kind, _)| kind == "compaction_end")
        .map(|(_, json)| json.clone())
        .expect("compaction_end event");
    assert_eq!(end["aborted"], serde_json::Value::Bool(false));
    assert!(
        end["errorMessage"]
            .as_str()
            .is_some_and(|message| message.contains("session storage was replaced")),
        "unexpected compaction_end errorMessage: {end}"
    );
    let plugin_failures = plugin_failures.lock().unwrap();
    assert_eq!(
        plugin_failures.len(),
        1,
        "the dropped append must emit exactly one session_compact_failed plugin event"
    );
    let failure = &plugin_failures[0].1;
    assert_eq!(failure["reason"], serde_json::json!("threshold"));
    assert_eq!(failure["aborted"], serde_json::Value::Bool(false));
    assert!(
        failure["errorMessage"]
            .as_str()
            .is_some_and(|message| message.contains("session storage was replaced")),
        "unexpected session_compact_failed payload: {failure}"
    );
    drop(plugin_failures);

    assert_no_compaction_entry(&second_path, "the replacement");
    let replacement = std::fs::read_to_string(&second_path).expect("replacement file");
    assert!(
        !replacement.contains("RETRY-SUMMARY"),
        "the retried compaction entered the replacement: {replacement}"
    );
    assert_eq!(
        persisted_message_roles(&second_path),
        vec![
            "user".to_owned(),
            "assistant".to_owned(),
            "user".to_owned(),
            "assistant".to_owned()
        ],
        "the replacement must keep only its own seeded history"
    );

    assert_no_compaction_entry(&first_path, "the outgoing transcript");
    let outgoing = std::fs::read_to_string(&first_path).expect("outgoing file");
    assert!(
        !outgoing.contains("RETRY-SUMMARY"),
        "the outgoing transcript must not receive the dropped compaction: {outgoing}"
    );
}

/// P2-03 residual control (in-loop threshold, NO swap): the run-start
/// identity wiring must not block an ordinary threshold compaction. The
/// prompt-start gate stays below the threshold; the first loop check (~487
/// tokens) exceeds the 469-token budget and must persist its summary on the
/// run's own transcript.
#[tokio::test]
async fn in_loop_threshold_compaction_still_succeeds_without_swap() {
    let temp = TempDir::new("inloop-threshold-normal");
    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("NORMAL-INLOOP SUMMARY")], StopReason::Stop);
        }
        assistant(vec![text("done")], StopReason::Stop)
    }));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    let path = session.path().to_path_buf();
    let mut model = fake_model();
    // Seed history ~408 tokens < 469; the ~315-char starting prompt pushes
    // the first loop turn to ~487, comfortably over the 469-token budget.
    model.context_window = 470;
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model,
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: tool_registry_with(None),
            session: Some(session),
            compaction: Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            }),
            max_turns: 2,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        Vec::new(),
    ));
    let session_log: SessionEventLog = Arc::new(Mutex::new(Vec::new()));
    agent.on_session_event(session_event_handler(Arc::clone(&session_log)));

    let prompt = format!("loop threshold {}", "z".repeat(300));
    let terminal = agent
        .prompt(&prompt, event_handler(EventLog::default()))
        .await
        .expect("the in-loop threshold compaction must not fail the run");
    assert_eq!(
        text_of_message(&Message::Assistant(terminal)).as_deref(),
        Some("done")
    );
    assert_eq!(
        compaction_event_kinds(&session_log),
        vec!["compaction_start".to_owned(), "compaction_end".to_owned()],
        "the normal in-loop threshold compaction must emit exactly one pair"
    );
    assert_eq!(
        persisted_compaction_entries(&path),
        1,
        "the run's own transcript must receive exactly one compaction"
    );
    let reopened = SessionManager::open(&path).expect("reopen session");
    let rendered = reopened
        .build_context_entries()
        .expect("context entries")
        .iter()
        .filter_map(text_of_message)
        .collect::<Vec<_>>();
    assert!(
        rendered
            .iter()
            .any(|text| text.contains("NORMAL-INLOOP SUMMARY")),
        "the in-loop compaction summary is missing after reopen: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|text| text == "done"),
        "the post-compaction assistant message is missing: {rendered:?}"
    );
}

// ---------------------------------------------------------------------------
// P1-1: `agent_settled` / `entry_appended` exactly once per face
// ---------------------------------------------------------------------------

/// Native counter for the exactly-once regression: records every
/// `agent_settled` / `entry_appended` payload the ordered host dispatches.
struct SettledEntryCounter {
    info: PluginInfo,
    seen: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
}

impl AgentPlugin for SettledEntryCounter {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::List(vec![
            "agent_settled".to_owned(),
            "entry_appended".to_owned(),
        ])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            seen.lock()
                .unwrap()
                .push((event.name.to_string(), event.payload.clone()));
            PluginEventOutcome::None
        })
    }
}

/// Both faces observe one `agent_settled` and each persisted entry's
/// `entry_appended` exactly once. The old session-event bridge forwarded the
/// same `SessionEvent`s on top of the direct plugin dispatch, so the default
/// host (JS + native) delivered every one of them twice.
#[tokio::test]
async fn agent_settled_and_entry_appended_reach_each_face_once_per_run() {
    let temp = TempDir::new("settled-entry-once");
    let handle = load_message_lifecycle_extension(
        "settled-entry-once",
        r#"
        export default function (pi) {
            globalThis.__counts = {};
            globalThis.__entryIds = [];
            for (const name of ["agent_settled", "entry_appended"]) {
                pi.on(name, (event) => {
                    globalThis.__counts[name] = (globalThis.__counts[name] ?? 0) + 1;
                    if (name === "entry_appended" && event.entry) {
                        globalThis.__entryIds.push(event.entry.id);
                    }
                });
            }
        }
        "#,
    )
    .await;
    let native_seen: Arc<Mutex<Vec<(String, serde_json::Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: Some(session),
            compaction: None,
            max_turns: 2,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: Some(handle.clone()),
            tool_selection: None,
        },
        vec![Arc::new(SettledEntryCounter {
            info: PluginInfo::inline("settled-entry-counter"),
            seen: Arc::clone(&native_seen),
        })],
    );

    agent
        .prompt("original", event_handler(EventLog::default()))
        .await
        .expect("prompt");

    // The session-event bridge dispatches on a separate ordered worker. Give
    // it time to drain so an accidental second delivery cannot hide behind
    // the test's exit.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let probe = handle
        .eval_json(
            "JSON.stringify({ counts: globalThis.__counts ?? {}, ids: globalThis.__entryIds ?? [] })",
        )
        .await
        .expect("probe JS counts");
    let js_counts: BTreeMap<String, usize> =
        serde_json::from_value(probe["counts"].clone()).expect("JS counts");
    let js_ids: Vec<String> = serde_json::from_value(probe["ids"].clone()).expect("JS entry ids");

    let native = native_seen.lock().unwrap();
    let native_settled = native
        .iter()
        .filter(|(name, _)| name == "agent_settled")
        .count();
    let native_ids: Vec<String> = native
        .iter()
        .filter(|(name, _)| name == "entry_appended")
        .filter_map(|(_, payload)| {
            payload
                .get("entry")
                .and_then(|entry| entry.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect();

    assert_eq!(
        js_counts.get("agent_settled").copied().unwrap_or(0),
        1,
        "JS `agent_settled` must fire exactly once: {js_counts:?}"
    );
    assert_eq!(
        native_settled, 1,
        "native `agent_settled` must fire exactly once: {native:?}"
    );
    assert_eq!(
        js_counts.get("entry_appended").copied().unwrap_or(0),
        js_ids.len(),
        "JS `entry_appended` count must match its recorded ids: {js_counts:?} / {js_ids:?}"
    );
    assert_eq!(
        native_ids, js_ids,
        "both faces must observe the same appended entries in the same order"
    );
    let mut unique_ids = js_ids.clone();
    unique_ids.sort();
    unique_ids.dedup();
    assert_eq!(
        unique_ids.len(),
        js_ids.len(),
        "duplicate `entry_appended` delivery: {js_ids:?}"
    );
    assert_eq!(
        js_ids.len(),
        native_ids.len(),
        "JS and native must observe the same number of appends: {js_ids:?} / {native_ids:?}"
    );
}

// ---------------------------------------------------------------------------
// P1-3: a JS-only plan receives the bridged session-level events
// ---------------------------------------------------------------------------

/// Poll the JS extension's `globalThis.__events` counts until `name` reaches
/// `expected` (the bridge dispatches on its ordered worker).
async fn wait_for_js_session_event(handle: &ExtensionHandle, name: &str, expected: usize) {
    for _ in 0..500 {
        if let Ok(probe) = handle
            .eval_json("JSON.stringify(globalThis.__events ?? {})")
            .await
        {
            let counts: BTreeMap<String, usize> = serde_json::from_value(probe).unwrap_or_default();
            if counts.get(name).copied().unwrap_or(0) >= expected {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("JS handler `{name}` never observed {expected} session event(s)");
}

/// `--no-extensions -e ext.ts` is a JS-only plugin plan: no non-JS plugin
/// exists, so the old bridge install gate (`has_non_js_plugins`) returned
/// `None` and `pi.on("compaction_start")` / `compaction_end` /
/// `background_tool_end` never fired. This drives a REAL threshold compaction
/// through a JS-only host, then the background completion through the same
/// `SessionEvent` seam, and asserts both JS handler counts.
#[tokio::test]
async fn js_only_plan_receives_compaction_and_background_session_events() {
    let temp = TempDir::new("js-only-session-events");
    let handle = load_message_lifecycle_extension(
        "js-only-session-events",
        r#"
        export default function (pi) {
            globalThis.__events = {};
            for (const name of ["compaction_start", "compaction_end", "background_tool_end"]) {
                pi.on(name, () => {
                    globalThis.__events[name] = (globalThis.__events[name] ?? 0) + 1;
                });
            }
        }
        "#,
    )
    .await;

    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("JS-ONLY SUMMARY")], StopReason::Stop);
        }
        assistant(vec![text("model reply")], StopReason::Stop)
    }));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    let mut model = fake_model();
    // Tiny window: the seeded history crosses the threshold at prompt start.
    model.context_window = 64;
    let agent = AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model,
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: Some(session),
            compaction: Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            }),
            max_turns: 2,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: Some(handle.clone()),
            tool_selection: None,
        },
        // JS-only: no inline native plugin, so `has_non_js_plugins()` is false.
        Vec::new(),
    );

    agent
        .prompt("trigger threshold", event_handler(EventLog::default()))
        .await
        .expect("the JS-only prompt completes");

    wait_for_js_session_event(&handle, "compaction_start", 1).await;
    wait_for_js_session_event(&handle, "compaction_end", 1).await;

    agent.emit_session_event_for_test(SessionEvent::BackgroundToolEnd {
        call_id: "bg-call".to_owned(),
        tool_name: "demo".to_owned(),
        parent_call_id: None,
        is_error: false,
        duration_ms: Some(5),
        output: "done".to_owned(),
        images: Vec::new(),
        details: None,
        structured_content: None,
        usage: None,
    });
    wait_for_js_session_event(&handle, "background_tool_end", 1).await;

    let probe = handle
        .eval_json("JSON.stringify(globalThis.__events ?? {})")
        .await
        .expect("probe JS session event counts");
    let counts: BTreeMap<String, usize> = serde_json::from_value(probe).expect("counts");
    assert_eq!(
        counts.get("compaction_start").copied().unwrap_or(0),
        1,
        "JS `compaction_start` must fire exactly once: {counts:?}"
    );
    assert_eq!(
        counts.get("compaction_end").copied().unwrap_or(0),
        1,
        "JS `compaction_end` must fire exactly once: {counts:?}"
    );
    assert_eq!(
        counts.get("background_tool_end").copied().unwrap_or(0),
        1,
        "JS `background_tool_end` must fire exactly once: {counts:?}"
    );
}

// ---------------------------------------------------------------------------
// P1-D1: the session-event bridge is installed at construction
// ---------------------------------------------------------------------------

/// Read the JS extension's recorded `bash_execution_update` deltas.
async fn js_bash_deltas(handle: &ExtensionHandle) -> Vec<String> {
    let probe = handle
        .eval_json("JSON.stringify(globalThis.__bashDeltas ?? [])")
        .await
        .expect("probe JS bash deltas");
    serde_json::from_value(probe).expect("JS bash deltas")
}

/// Poll until the recorded bash deltas concatenate to `expected` (the bridge
/// dispatches on its ordered worker, not on the emitting task). Equality (not
/// a substring check) also catches a duplicate subscription, which would hand
/// the handler each chunk twice.
async fn wait_for_js_bash_output(handle: &ExtensionHandle, expected: &str) {
    for _ in 0..500 {
        if js_bash_deltas(handle).await.concat() == expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "JS handler never observed the full bash output `{expected}`: {:?}",
        js_bash_deltas(handle).await
    );
}

/// Read the JS extension's session-event counts.
async fn js_session_event_counts(handle: &ExtensionHandle) -> BTreeMap<String, usize> {
    let probe = handle
        .eval_json("JSON.stringify(globalThis.__events ?? {})")
        .await
        .expect("probe JS session event counts");
    serde_json::from_value(probe).expect("JS session event counts")
}

/// A JS-only plan must already have the `SessionEvent` bridge when a public
/// API emits before the first prompt/idle drain: `compact()` and
/// `execute_bash()` emit `compaction_start`/`_end` and
/// `bash_execution_update` directly (the manual compaction also owns the
/// `summarization_retry_*` lifecycle). The old lazy installer only ran in
/// `start_run` / the idle drain, so `pi.on` saw none of these until a run.
///
/// The post-run half pins idempotence: `start_run` still calls the installer,
/// which must NOT add a second subscriber (that would double every later
/// compaction/bash event). The counts and the byte-for-byte delta
/// concatenation catch both a missing install and a duplicate one.
#[tokio::test]
async fn js_only_manual_compact_and_bash_before_first_run_reach_pi_on_exactly_once() {
    let temp = TempDir::new("js-only-pre-run-events");
    let handle = load_message_lifecycle_extension(
        "js-only-pre-run-events",
        r#"
        export default function (pi) {
            globalThis.__events = {};
            globalThis.__bashDeltas = [];
            for (const name of ["compaction_start", "compaction_end", "bash_execution_update"]) {
                pi.on(name, (event) => {
                    globalThis.__events[name] = (globalThis.__events[name] ?? 0) + 1;
                    if (name === "bash_execution_update" && typeof event.delta === "string") {
                        globalThis.__bashDeltas.push(event.delta);
                    }
                });
            }
        }
        "#,
    )
    .await;

    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("PRE-RUN COMPACT SUMMARY")], StopReason::Stop);
        }
        assistant(vec![text("model reply")], StopReason::Stop)
    }));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    let mut model = fake_model();
    // The manual compaction is the only one in play: a huge window keeps the
    // prompt-start threshold hook idle.
    model.context_window = 1_000_000;
    let agent = AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model,
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: Some(session),
            compaction: Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            }),
            max_turns: 2,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: Some(handle.clone()),
            tool_selection: None,
        },
        // JS-only: no inline native plugin -> `has_non_js_plugins()` is false.
        Vec::new(),
    );

    // NO prompt yet: the manual compaction must already reach `pi.on`.
    agent
        .compact(None)
        .await
        .expect("manual compact before the first run");
    wait_for_js_session_event(&handle, "compaction_start", 1).await;
    wait_for_js_session_event(&handle, "compaction_end", 1).await;

    // NO prompt yet: the bash stream must already reach `pi.on`.
    let pre_run = agent.execute_bash("echo lane-dispatch2-pre-run").await;
    assert_eq!(pre_run.exit_code, Some(0), "pre-run bash: {pre_run:?}");
    wait_for_js_bash_output(&handle, &pre_run.output).await;
    let pre_run_deltas = js_bash_deltas(&handle).await;
    assert_eq!(
        pre_run_deltas.concat(),
        pre_run.output,
        "pre-run bash chunks must be delivered exactly once: {pre_run_deltas:?}"
    );

    // First run: its idempotent install must not add a second subscriber.
    agent
        .prompt("first run", event_handler(EventLog::default()))
        .await
        .expect("prompt");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let counts_after_run = js_session_event_counts(&handle).await;
    assert_eq!(
        counts_after_run
            .get("compaction_start")
            .copied()
            .unwrap_or(0),
        1,
        "the first run must not duplicate the pre-run compaction_start: {counts_after_run:?}"
    );
    assert_eq!(
        counts_after_run.get("compaction_end").copied().unwrap_or(0),
        1,
        "the first run must not duplicate the pre-run compaction_end: {counts_after_run:?}"
    );

    // Post-run producers still deliver exactly once (no second subscriber).
    let post_run = agent.execute_bash("echo lane-dispatch2-post-run").await;
    assert_eq!(post_run.exit_code, Some(0), "post-run bash: {post_run:?}");
    wait_for_js_bash_output(&handle, &format!("{}{}", pre_run.output, post_run.output)).await;
    let all_deltas = js_bash_deltas(&handle).await;
    assert_eq!(
        all_deltas[pre_run_deltas.len()..].concat(),
        post_run.output,
        "post-run bash chunks must be delivered exactly once: {all_deltas:?}"
    );

    // `compact()` emits its start/end pair even when the post-run transcript
    // has nothing left to summarize; the point is the exactly-once delivery of
    // the second invocation after the run path's installer call.
    let _ = agent.compact(None).await;
    wait_for_js_session_event(&handle, "compaction_start", 2).await;
    wait_for_js_session_event(&handle, "compaction_end", 2).await;

    let counts = js_session_event_counts(&handle).await;
    assert_eq!(
        counts.get("compaction_start").copied().unwrap_or(0),
        2,
        "two manual compactions must deliver two compaction_start events, not more: {counts:?}"
    );
    assert_eq!(
        counts.get("compaction_end").copied().unwrap_or(0),
        2,
        "two manual compactions must deliver two compaction_end events, not more: {counts:?}"
    );
}

// ---------------------------------------------------------------------------
// P2-D1: one ordered compaction lifecycle across both delivery channels
// ---------------------------------------------------------------------------

/// Extension recording the four compaction lifecycle events `pi.on` observes.
/// `compaction_start`/`compaction_end` arrive through the ordered
/// `SessionEvent` backlog; `session_before_compact`/`session_compact` are
/// dispatched directly by the compaction task. The shared array pins their
/// relative order.
const COMPACTION_ORDER_EXTENSION: &str = r#"
    export default function (pi) {
        globalThis.__compactionOrder = [];
        for (const name of [
            "compaction_start",
            "session_before_compact",
            "session_compact",
            "compaction_end",
        ]) {
            pi.on(name, () => {
                globalThis.__compactionOrder.push(name);
            });
        }
        // Delay the bridge worker long enough that a direct
        // `session_before_compact` dispatch without a queue flush overtakes
        // the queued `compaction_start` on the JS actor. Only the first
        // update pays the delay (a threshold prompt emits several).
        pi.on("queue_update", () => {
            if (globalThis.__queueDelayed) return;
            globalThis.__queueDelayed = true;
            const until = Date.now() + 100;
            while (Date.now() < until) {}
        });
    }
"#;

async fn js_compaction_order(handle: &ExtensionHandle) -> Vec<String> {
    let probe = handle
        .eval_json("JSON.stringify(globalThis.__compactionOrder ?? [])")
        .await
        .expect("probe JS compaction order");
    serde_json::from_value(probe).expect("JS compaction order")
}

async fn wait_for_js_compaction_order(handle: &ExtensionHandle, expected: usize) -> Vec<String> {
    for _ in 0..500 {
        let order = js_compaction_order(handle).await;
        if order.len() >= expected {
            return order;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "JS compaction lifecycle never reached {expected} events: {:?}",
        js_compaction_order(handle).await
    );
}

/// Both faces must see `compaction_start → session_before_compact →
/// session_compact → compaction_end` even when the JS runtime is the only
/// plugin face and the bridge is the only `SessionEvent` subscriber. Before
/// the flush barrier the start event could be dispatched after the direct
/// `session_before_compact` emit.
#[tokio::test]
async fn js_only_manual_compaction_orders_its_lifecycle_events() {
    let temp = TempDir::new("js-only-manual-compaction-order");
    let handle =
        load_message_lifecycle_extension("js-only-manual-order", COMPACTION_ORDER_EXTENSION).await;

    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("MANUAL ORDER SUMMARY")], StopReason::Stop);
        }
        assistant(vec![text("model reply")], StopReason::Stop)
    }));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    let mut model = fake_model();
    // A huge window keeps the automatic threshold hook idle: only the manual
    // compaction under test runs.
    model.context_window = 1_000_000;
    let agent = AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model,
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: Some(session),
            compaction: Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            }),
            max_turns: 2,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: Some(handle.clone()),
            tool_selection: None,
        },
        // JS-only: no inline native plugin.
        Vec::new(),
    );

    // Occupy the bridge worker with a JS handler for 100 ms, so the queued
    // `compaction_start` cannot win merely by being scheduled first: without
    // the flush barrier the direct `session_before_compact` emit reaches the
    // JS actor first and the asserted order goes red.
    agent.emit_session_event_for_test(SessionEvent::QueueUpdate {
        steering: Vec::new(),
        follow_up: Vec::new(),
        delivered_steering: Vec::new(),
        delivered_follow_up: Vec::new(),
    });

    agent.compact(None).await.expect("manual compact");

    let expected = vec![
        "compaction_start".to_owned(),
        "session_before_compact".to_owned(),
        "session_compact".to_owned(),
        "compaction_end".to_owned(),
    ];
    let order = wait_for_js_compaction_order(&handle, expected.len()).await;
    assert_eq!(
        order, expected,
        "JS-only manual compaction must preserve the upstream lifecycle order"
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        js_compaction_order(&handle).await,
        expected,
        "the lifecycle events must be delivered exactly once"
    );
}

/// Threshold counterpart: the prompt-start/per-turn threshold hook uses the
/// same two delivery channels, so a JS-only face must observe the identical
/// order without a manual trigger.
#[tokio::test]
async fn js_only_threshold_compaction_orders_its_lifecycle_events() {
    let temp = TempDir::new("js-only-threshold-compaction-order");
    let handle =
        load_message_lifecycle_extension("js-only-threshold-order", COMPACTION_ORDER_EXTENSION)
            .await;

    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("THRESHOLD ORDER SUMMARY")], StopReason::Stop);
        }
        assistant(vec![text("model reply")], StopReason::Stop)
    }));
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    seed_compactable_history(&mut session);
    let mut model = fake_model();
    // Tiny window: the seeded history crosses the threshold at prompt start.
    model.context_window = 64;
    let agent = AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model,
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: Some(session),
            compaction: Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            }),
            max_turns: 2,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: Some(handle.clone()),
            tool_selection: None,
        },
        Vec::new(),
    );

    agent
        .prompt("trigger threshold", event_handler(EventLog::default()))
        .await
        .expect("the JS-only threshold prompt completes");

    let expected = vec![
        "compaction_start".to_owned(),
        "session_before_compact".to_owned(),
        "session_compact".to_owned(),
        "compaction_end".to_owned(),
    ];
    let order = wait_for_js_compaction_order(&handle, expected.len()).await;
    assert_eq!(
        order, expected,
        "JS-only threshold compaction must preserve the upstream lifecycle order"
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        js_compaction_order(&handle).await,
        expected,
        "the lifecycle events must be delivered exactly once"
    );
}
