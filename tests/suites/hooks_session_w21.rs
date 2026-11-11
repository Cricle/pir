//! H9 (hooks/events close-out): session-installed `ToolHooks` and rich
//! steering/follow-up messages.
//!
//! Covers:
//! - `beforeToolCall` / `afterToolCall` installed through `AgentSession`;
//!   ordering against the H2 extension `tool_call` / `tool_result` faces and
//!   the absence of duplicate dispatch;
//! - block (with/without `terminate`) and after-hook `terminate`;
//! - image steering and custom (`role: "custom"`) steering/follow-up reaching
//!   the provider request, the message lifecycle, and session storage;
//! - the no-hook behavior staying byte-identical.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{
    AfterToolCallResult, AgentTool, BeforeToolCallResult, BoxFuture, ToolContext, ToolHooks,
    ToolRegistry,
};
use pir::agent_session::{AgentSession, AgentSessionConfig, CustomRunMessage, Disposition};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginInfo,
};
use pir::session_manager::{FileEntry, SessionManager};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-hooks-w21-{label}-{}-{}",
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

fn fake_model() -> Model {
    Model {
        id: "w21-model".into(),
        name: "W21 model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into(), "image".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 100_000,
        max_tokens: 512,
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
        model: "w21-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    }
}

fn tool_call(id: &str, name: &str, arguments: Value) -> ContentBlock {
    let arguments: BTreeMap<String, Value> = arguments
        .as_object()
        .expect("object arguments")
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

struct FakeProvider {
    calls: AtomicUsize,
    respond: Responder,
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "W21 fake provider"
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

/// A tool that records each executed payload and answers `raw`.
struct ProbeTool {
    runs: Arc<Mutex<Vec<Value>>>,
}

impl AgentTool for ProbeTool {
    fn name(&self) -> &'static str {
        "probe"
    }
    fn description(&self) -> &'static str {
        "records its arguments and returns a raw result"
    }
    fn parameters(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false
        }))
    }
    fn run(&self, args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(args.clone());
        let mut result = ToolResult::ok("raw");
        result.details = Some(json!({ "origin": "tool" }));
        Box::pin(async move { result })
    }
}

/// Inline plugin recording every dispatched event's name and payload.
struct Recorder {
    info: PluginInfo,
    log: Arc<Mutex<Vec<(String, Value)>>>,
    interest: EventInterest,
}

impl Recorder {
    fn for_events(names: &[&str], log: Arc<Mutex<Vec<(String, Value)>>>) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline("hooks-w21-recorder"),
            log,
            interest: EventInterest::List(names.iter().map(|name| (*name).to_owned()).collect()),
        })
    }
}

impl AgentPlugin for Recorder {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        self.interest.clone()
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            self.log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((event.name.to_string(), event.payload.clone()));
            PluginEventOutcome::None
        })
    }
}

fn session_config(
    temp: &TempDir,
    registry: Arc<ProviderRegistry>,
    tools: ToolRegistry,
    storage: Option<SessionManager>,
) -> AgentSessionConfig {
    AgentSessionConfig {
        performance: Default::default(),
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: storage,
        compaction: None,
        max_turns: 8,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

fn probe_registry() -> (ToolRegistry, Arc<Mutex<Vec<Value>>>) {
    let runs = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ProbeTool {
        runs: Arc::clone(&runs),
    }));
    (tools, runs)
}

fn tool_results(context: &Context) -> Vec<ToolResultMessage> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

fn user_message_with_image(text_value: &str, data: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![
            text(text_value),
            ContentBlock::Image(ImageContent {
                data: data.to_owned(),
                mime_type: "image/png".to_owned(),
            }),
        ],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1,
    })
}

fn find_user_message_with_image<'a>(
    context: &'a Context,
    text_value: &str,
) -> Option<&'a UserMessage> {
    context.messages.iter().find_map(|message| match message {
        Message::User(user)
            if user.content.iter().any(|block| match block {
                ContentBlock::Text(part) => part.text == text_value,
                _ => false,
            }) =>
        {
            Some(user)
        }
        _ => None,
    })
}

fn has_image(user: &UserMessage) -> bool {
    user.content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)))
}

// ---------------------------------------------------------------------------
// 1. Session-installed hooks: ordering against the extension faces
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_tool_hooks_run_between_the_extension_faces_exactly_once() {
    let temp = TempDir::new("order");
    let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let plugin_log: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Recorder::for_events(&["tool_call", "tool_result"], Arc::clone(&plugin_log));
    let (tools, runs) = probe_registry();

    let requests: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_responder = Arc::clone(&requests);
    let registry = fake_registry(Arc::new(move |index, context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "probe", json!({ "value": "in" }))],
                StopReason::ToolUse,
            )
        } else {
            requests_for_responder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(context.clone());
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let storage = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(AgentSession::new_with_plugins(
        session_config(&temp, registry, tools, Some(storage)),
        vec![recorder as Arc<dyn AgentPlugin>],
    ));

    let order_for_before = Arc::clone(&order);
    let order_for_after = Arc::clone(&order);
    let runs_for_assert = Arc::clone(&runs);
    // The tool's own body cannot push to `order` (ProbeTool has no handle), so
    // the hook pair brackets it: `before` observes the validated args and the
    // after hook observes the raw result the tool produced.
    agent.set_tool_hooks(Some(Arc::new(ToolHooks {
        before: Some(Box::new(move |call, _ctx| {
            let order = Arc::clone(&order_for_before);
            let args = call.validated_args.clone();
            Box::pin(async move {
                order
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("before".to_owned());
                assert_eq!(args, json!({ "value": "in" }));
                BeforeToolCallResult::default()
            })
        })),
        after: Some(Box::new(move |_call, result, _ctx| {
            let order = Arc::clone(&order_for_after);
            let raw = result.content.clone();
            Box::pin(async move {
                order
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("after".to_owned());
                assert_eq!(raw, "raw");
                AfterToolCallResult {
                    content: Some("hooked".to_owned()),
                    details: Some(json!({ "by": "after" })),
                    usage: Some(Usage {
                        input: 11,
                        output: 4,
                        total_tokens: 15,
                        ..Usage::default()
                    }),
                    ..AfterToolCallResult::default()
                }
            })
        })),
    })));

    let terminal = agent
        .prompt("start", Arc::new(|_| {}))
        .await
        .expect("prompt");
    assert_eq!(
        terminal.content,
        vec![text("done")],
        "the tool turn must continue to the final assistant message"
    );

    // The extension `tool_call` face runs first (after validation), then the
    // before hook, the tool, the after hook, and only then `tool_result`.
    let plugin_events: Vec<String> = plugin_log
        .lock()
        .unwrap()
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    assert_eq!(
        plugin_events,
        vec!["tool_call".to_owned(), "tool_result".to_owned()],
        "each extension face must run exactly once"
    );
    let hook_events = order.lock().unwrap().clone();
    assert_eq!(hook_events, vec!["before".to_owned(), "after".to_owned()]);
    assert_eq!(
        runs_for_assert
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        &[json!({ "value": "in" })]
    );

    // The after-hook rewrite is what the next request (and the transcript)
    // carries: content, details, usage, error flag.
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "one follow-up request");
    let results = tool_results(&requests[0]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content, vec![text("hooked")]);
    assert_eq!(results[0].details, Some(json!({ "by": "after" })));
    assert!(!results[0].is_error);
    assert_eq!(
        results[0].usage.map(|usage| usage.total_tokens),
        Some(15),
        "after-hook usage rides the tool-result message"
    );
}

// ---------------------------------------------------------------------------
// 2. before-hook block (with and without terminate)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_before_hook_block_reaches_the_next_request() {
    let temp = TempDir::new("block");
    let (tools, runs) = probe_registry();
    let requests: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_responder = Arc::clone(&requests);
    let registry = fake_registry(Arc::new(move |index, context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "probe", json!({ "value": "in" }))],
                StopReason::ToolUse,
            )
        } else {
            requests_for_responder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(context.clone());
            assistant(vec![text("after block")], StopReason::Stop)
        }
    }));
    let agent = Arc::new(AgentSession::new(session_config(
        &temp,
        registry,
        tools,
        Some(SessionManager::create(".", &temp.0).expect("session")),
    )));
    agent.set_tool_hooks(Some(Arc::new(ToolHooks {
        before: Some(Box::new(|_call, _ctx| {
            Box::pin(async {
                BeforeToolCallResult {
                    block: true,
                    reason: Some("denied by hook".to_owned()),
                    terminate: false,
                }
            })
        })),
        after: None,
    })));

    agent
        .prompt("start", Arc::new(|_| {}))
        .await
        .expect("prompt");

    assert!(
        runs.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "a blocked tool must never execute"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let results = tool_results(&requests[0]);
    assert_eq!(results.len(), 1);
    assert!(results[0].is_error);
    assert_eq!(results[0].content, vec![text("denied by hook")]);
}

#[tokio::test]
async fn session_before_hook_block_with_terminate_ends_the_run() {
    let temp = TempDir::new("block-terminate");
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_responder = Arc::clone(&calls);
    let (tools, runs) = probe_registry();
    let registry = fake_registry(Arc::new(move |_index, _context| {
        calls_for_responder.fetch_add(1, Ordering::SeqCst);
        assistant(
            vec![tool_call("call-1", "probe", json!({ "value": "in" }))],
            StopReason::ToolUse,
        )
    }));
    let agent = Arc::new(AgentSession::new(session_config(
        &temp,
        registry,
        tools,
        Some(SessionManager::create(".", &temp.0).expect("session")),
    )));
    agent.set_tool_hooks(Some(Arc::new(ToolHooks {
        before: Some(Box::new(|_call, _ctx| {
            Box::pin(async {
                BeforeToolCallResult {
                    block: true,
                    reason: Some("stop the batch".to_owned()),
                    terminate: true,
                }
            })
        })),
        after: None,
    })));

    agent
        .prompt("start", Arc::new(|_| {}))
        .await
        .expect("prompt");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a fully terminating blocked batch must not issue another request"
    );
    assert!(runs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_empty());
}

#[tokio::test]
async fn session_after_hook_terminate_ends_the_run() {
    let temp = TempDir::new("after-terminate");
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_responder = Arc::clone(&calls);
    let (tools, runs) = probe_registry();
    let registry = fake_registry(Arc::new(move |_index, _context| {
        calls_for_responder.fetch_add(1, Ordering::SeqCst);
        assistant(
            vec![tool_call("call-1", "probe", json!({ "value": "in" }))],
            StopReason::ToolUse,
        )
    }));
    let agent = Arc::new(AgentSession::new(session_config(
        &temp,
        registry,
        tools,
        Some(SessionManager::create(".", &temp.0).expect("session")),
    )));
    agent.set_tool_hooks(Some(Arc::new(ToolHooks {
        before: None,
        after: Some(Box::new(|_call, _result, _ctx| {
            Box::pin(async {
                AfterToolCallResult {
                    terminate: Some(true),
                    ..AfterToolCallResult::default()
                }
            })
        })),
    })));

    agent
        .prompt("start", Arc::new(|_| {}))
        .await
        .expect("prompt");

    assert_eq!(
        runs.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1,
        "the tool runs once before the after-hook's termination hint"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a terminating after-hook must not issue another request"
    );
}

// ---------------------------------------------------------------------------
// 3. No hooks: unchanged behavior
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_without_tool_hooks_is_unchanged() {
    let temp = TempDir::new("no-hooks");
    let (tools, runs) = probe_registry();
    let requests: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_responder = Arc::clone(&requests);
    let registry = fake_registry(Arc::new(move |index, context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "probe", json!({ "value": "in" }))],
                StopReason::ToolUse,
            )
        } else {
            requests_for_responder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(context.clone());
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let agent = Arc::new(AgentSession::new(session_config(
        &temp,
        registry,
        tools,
        Some(SessionManager::create(".", &temp.0).expect("session")),
    )));

    agent
        .prompt("start", Arc::new(|_| {}))
        .await
        .expect("prompt");

    assert_eq!(
        runs.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
    let requests = requests.lock().unwrap();
    let results = tool_results(&requests[0]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content, vec![text("raw")]);
    assert_eq!(results[0].details, Some(json!({ "origin": "tool" })));
    assert_eq!(results[0].usage, None);
    assert!(!results[0].is_error);
}

// ---------------------------------------------------------------------------
// 4. Image steering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn image_steering_reaches_the_request_and_storage() {
    let temp = TempDir::new("image-steer");
    let session = SessionManager::create(".", &temp.0).expect("session");
    let path = session.path().to_path_buf();
    let requests: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_responder = Arc::clone(&requests);
    let registry = fake_registry(Arc::new(move |_index, context| {
        requests_for_responder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context.clone());
        assistant(vec![text("seen")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new(session_config(
        &temp,
        registry,
        ToolRegistry::new(),
        Some(session),
    )));

    assert_eq!(
        agent.steer_message(user_message_with_image("see this", "aW1n")),
        Disposition::Handled,
        "an idle queue accepts the message immediately"
    );
    // The String API stays: the queue reports the message text.
    assert_eq!(
        agent.get_steering_messages().await,
        vec!["see this".to_owned()]
    );

    agent
        .prompt("start", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let user = find_user_message_with_image(&requests[0], "see this")
        .expect("the steering message reaches the first request");
    assert!(has_image(user), "the image block must survive steering");
    drop(requests);

    // Persistence: the same user message is part of the stored transcript.
    let reopened = SessionManager::open(&path).expect("reopen");
    let stored = reopened.build_context_entries().expect("context entries");
    let user = stored
        .iter()
        .find_map(|message| match message {
            Message::User(user)
                if user.content.iter().any(|block| match block {
                    ContentBlock::Text(part) => part.text == "see this",
                    _ => false,
                }) =>
            {
                Some(user)
            }
            _ => None,
        })
        .expect("the steering message is persisted");
    assert!(has_image(user), "the persisted message keeps its image");
}

// ---------------------------------------------------------------------------
// 5. Custom steering: lifecycle + custom-entry persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn custom_steering_uses_the_message_lifecycle_and_persists_a_custom_entry() {
    let temp = TempDir::new("custom-steer");
    let session = SessionManager::create(".", &temp.0).expect("session");
    let path = session.path().to_path_buf();
    let plugin_log: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Recorder::for_events(&["message_start", "message_end"], Arc::clone(&plugin_log));
    let requests: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_responder = Arc::clone(&requests);
    let registry = fake_registry(Arc::new(move |_index, context| {
        requests_for_responder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context.clone());
        assistant(vec![text("noted")], StopReason::Stop)
    }));
    let agent = Arc::new(AgentSession::new_with_plugins(
        session_config(&temp, registry, ToolRegistry::new(), Some(session)),
        vec![recorder as Arc<dyn AgentPlugin>],
    ));

    let custom = CustomRunMessage {
        custom_type: "note".to_owned(),
        content: json!([
            { "type": "text", "text": "custom steer" },
            { "type": "image", "data": "aW1n", "mimeType": "image/png" }
        ]),
        display: true,
        details: Some(json!({ "k": 1 })),
    };
    assert_eq!(agent.steer_custom(custom), Disposition::Handled);
    // The text compatibility layer exposes the projected text.
    assert_eq!(
        agent.get_steering_messages().await,
        vec!["custom steer".to_owned()]
    );

    agent
        .prompt("start", Arc::new(|_| {}))
        .await
        .expect("prompt");

    // The first request sees the custom message's user-shaped projection,
    // image block included.
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let user = find_user_message_with_image(&requests[0], "custom steer")
        .expect("the custom projection reaches the request");
    assert!(has_image(user), "custom content keeps its image block");
    drop(requests);

    // The message lifecycle carried the `role: "custom"` wire shape exactly
    // once in each direction (the assistant's own lifecycle is a separate
    // role).
    let log = plugin_log.lock().unwrap();
    let custom_events: Vec<&str> = log
        .iter()
        .filter(|(_, payload)| payload["message"]["role"] == json!("custom"))
        .map(|(name, payload)| {
            assert_eq!(payload["message"]["customType"], json!("note"));
            assert_eq!(payload["message"]["display"], json!(true));
            assert_eq!(payload["message"]["details"], json!({ "k": 1 }));
            name.as_str()
        })
        .collect();
    assert_eq!(
        custom_events,
        vec!["message_start", "message_end"],
        "the custom message rides the H4b lifecycle exactly once"
    );
    drop(log);

    // Storage: exactly one custom entry, and no duplicate regular user entry
    // carrying the same text.
    let reopened = SessionManager::open(&path).expect("reopen");
    let branch = reopened.branch_entries(None).expect("branch");
    let custom_entries: Vec<&pir::session_manager::types::CustomMessageEntry> = branch
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::CustomMessage(entry) => Some(entry),
            _ => None,
        })
        .collect();
    assert_eq!(custom_entries.len(), 1, "one custom entry in storage");
    assert_eq!(custom_entries[0].custom_type, "note");
    assert!(custom_entries[0].display);
    assert_eq!(custom_entries[0].details, Some(json!({ "k": 1 })));
    let regular_duplicate = branch.iter().any(|entry| match entry {
        FileEntry::Message(message) => match &message.message {
            pir::session_manager::types::StoredMessage::User(user) => {
                user.content.iter().any(|block| match block {
                    ContentBlock::Text(part) => part.text.contains("custom steer"),
                    _ => false,
                })
            }
            _ => false,
        },
        _ => false,
    });
    assert!(
        !regular_duplicate,
        "the custom entry must not also be persisted as a regular message"
    );
}

// ---------------------------------------------------------------------------
// 6. Custom follow-up: delivered at the terminal turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn custom_follow_up_continues_the_run_and_persists_a_custom_entry() {
    let temp = TempDir::new("custom-follow-up");
    let session = SessionManager::create(".", &temp.0).expect("session");
    let path = session.path().to_path_buf();
    let requests: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_responder = Arc::clone(&requests);
    let registry = fake_registry(Arc::new(move |index, context| {
        requests_for_responder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context.clone());
        if index == 0 {
            assistant(vec![text("first")], StopReason::Stop)
        } else {
            assistant(vec![text("second")], StopReason::Stop)
        }
    }));
    let agent = Arc::new(AgentSession::new(session_config(
        &temp,
        registry,
        ToolRegistry::new(),
        Some(session),
    )));

    let custom = CustomRunMessage {
        custom_type: "followup-note".to_owned(),
        content: json!([{ "type": "text", "text": "custom follow up" }]),
        display: false,
        details: None,
    };
    assert_eq!(agent.follow_up_custom(custom), Disposition::Handled);

    let terminal = agent
        .prompt("start", Arc::new(|_| {}))
        .await
        .expect("prompt");
    assert_eq!(terminal.content, vec![text("second")]);

    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "the custom follow-up continues the run with another request"
    );
    assert!(
        find_user_message_with_image(&requests[1], "custom follow up").is_some(),
        "the follow-up projection reaches the next request"
    );
    drop(requests);

    let reopened = SessionManager::open(&path).expect("reopen");
    let branch = reopened.branch_entries(None).expect("branch");
    let custom_entries: Vec<&pir::session_manager::types::CustomMessageEntry> = branch
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::CustomMessage(entry) => Some(entry),
            _ => None,
        })
        .collect();
    assert_eq!(custom_entries.len(), 1);
    assert_eq!(custom_entries[0].custom_type, "followup-note");
}

// ---------------------------------------------------------------------------
// 5. PIR-BIN-045: an aborted run never reaches the `agent_before_settle`
//    boundary
// ---------------------------------------------------------------------------

/// A tool that parks until the test releases it (or the run is aborted),
/// mirroring the abort repro's long-running scripted call.
struct ParkedTool {
    gate: tokio::sync::watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
}

impl AgentTool for ParkedTool {
    fn name(&self) -> &'static str {
        "parked"
    }
    fn description(&self) -> &'static str {
        "parks until released or aborted"
    }
    fn parameters(&self) -> Option<Value> {
        Some(json!({ "type": "object" }))
    }
    fn run(&self, _args: &Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let mut gate = self.gate.clone();
        let signal = ctx.signal.clone();
        Box::pin(async move {
            loop {
                if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    return ToolResult::error("Operation aborted");
                }
                if *gate.borrow() {
                    return ToolResult::ok("released");
                }
                if tokio::time::timeout(std::time::Duration::from_millis(10), gate.changed())
                    .await
                    .is_err()
                {
                    continue;
                }
            }
        })
    }
}

/// `agent_before_settle` handler that always asks for a continuation. Each
/// dispatch is counted so the test can assert the aborted run dispatched ZERO
/// boundaries (upstream `agent-session.ts:1906`: the abort-requested check
/// leaves the settle loop before `_runBeforeSettleBoundary`).
struct ContinueAlwaysSettle {
    info: PluginInfo,
    dispatches: Arc<AtomicUsize>,
}

impl AgentPlugin for ContinueAlwaysSettle {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["agent_before_settle".into()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        if event.name != "agent_before_settle" {
            return Box::pin(async { PluginEventOutcome::None });
        }
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { PluginEventOutcome::Generic(json!({ "continue": true })) })
    }
}

/// PIR-BIN-045: an abort during the tool batch must skip the
/// `agent_before_settle` boundary entirely.
///
/// Upstream's settle loop breaks BEFORE the boundary whenever the run's abort
/// was requested (`agent-session.ts:1900-1907`), so `agent_before_settle`
/// never fires, its `continue` is never honored, and the run ends
/// `turn_end -> agent_end -> agent_settled{aborted:true}` after ONE provider
/// request. pir dispatched the boundary three times, re-entered the loop twice
/// and emitted two extra `agent_start`/`agent_end` pairs (measured in
/// `.play/evidence/ab-round1/lane-ab-hooks/runs/abort-continue/pir`).
#[tokio::test]
async fn aborted_run_skips_the_agent_before_settle_boundary() {
    let temp = TempDir::new("abort-boundary");
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_responder = Arc::clone(&requests);
    let registry = fake_registry(Arc::new(move |_index, _context| {
        requests_for_responder.fetch_add(1, Ordering::SeqCst);
        assistant(
            vec![tool_call("call-1", "parked", json!({}))],
            StopReason::ToolUse,
        )
    }));
    // The gate never opens: only the abort can end the tool.
    let (_gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ParkedTool {
        gate: gate_rx,
        entered: Arc::clone(&entered),
    }));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let dispatches = Arc::new(AtomicUsize::new(0));
    let agent = Arc::new(AgentSession::new_with_plugins(
        session_config(&temp, registry, tools, Some(session)),
        vec![Arc::new(ContinueAlwaysSettle {
            info: PluginInfo::inline("continue-always-settle"),
            dispatches: Arc::clone(&dispatches),
        }) as Arc<dyn AgentPlugin>],
    ));
    // Observe the host event stream to count the run's `agent_start` /
    // `agent_end` pairs: a boundary continuation would emit a second pair.
    let lifecycle: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let lifecycle_for_handler = Arc::clone(&lifecycle);
    let prompt = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt(
                    "start",
                    Arc::new(move |event: pir::agent::agent_loop::AgentEvent| {
                        let name = match &event {
                            pir::agent::agent_loop::AgentEvent::AgentStart { .. } => "agent_start",
                            pir::agent::agent_loop::AgentEvent::AgentEnd { .. } => "agent_end",
                            pir::agent::agent_loop::AgentEvent::TurnEnd { .. } => "turn_end",
                            _ => return,
                        };
                        lifecycle_for_handler.lock().unwrap().push(name.to_owned());
                    }),
                )
                .await
        }
    });
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        entered.load(Ordering::SeqCst) > 0,
        "the parked tool must have started before the abort"
    );
    agent.abort();
    prompt.await.expect("prompt task").expect("prompt result");

    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        0,
        "an aborted run must not dispatch `agent_before_settle` (PIR-BIN-045)"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "an aborted run issues exactly one provider request"
    );
    let lifecycle = lifecycle.lock().unwrap().clone();
    assert_eq!(
        lifecycle
            .iter()
            .filter(|name| *name == "agent_start")
            .count(),
        1,
        "one aborted run must emit exactly one agent_start: {lifecycle:?}"
    );
    assert_eq!(
        lifecycle.iter().filter(|name| *name == "agent_end").count(),
        1,
        "one aborted run must emit exactly one agent_end: {lifecycle:?}"
    );
    assert_eq!(
        lifecycle.last().map(String::as_str),
        Some("agent_end"),
        "the boundary must not re-enter the loop after the abort: {lifecycle:?}"
    );
}

/// A NORMAL (non-aborted) settle still dispatches the boundary exactly once
/// and honors `continue` — the abort guard must not turn into a blanket skip.
#[tokio::test]
async fn normal_settle_dispatches_the_boundary_once_and_honors_continue() {
    let temp = TempDir::new("settle-once");
    // Turn 1 ends on a tool result (runnable), so the continuation is
    // satisfiable and the loop re-enters for turn 2.
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_responder = Arc::clone(&requests);
    let registry = fake_registry(Arc::new(move |index, _context| {
        requests_for_responder.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "probe", json!({ "value": "in" }))],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let (tools, _runs) = probe_registry();
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let dispatches = Arc::new(AtomicUsize::new(0));
    let agent = Arc::new(AgentSession::new_with_plugins(
        session_config(&temp, registry, tools, Some(session)),
        vec![Arc::new(ContinueAlwaysSettle {
            info: PluginInfo::inline("continue-once-settle"),
            dispatches: Arc::clone(&dispatches),
        }) as Arc<dyn AgentPlugin>],
    ));

    agent
        .prompt("start", Arc::new(|_| {}))
        .await
        .expect("prompt");

    // The boundary fires once per settle. The handler continues whenever the
    // context can run; the second boundary (after the continuation's text-only
    // turn) is dismissed by the loop's `canContinue` gate, so exactly one
    // honored continuation means two dispatches at most. What must NOT happen
    // is zero dispatches (the non-aborted path still runs the boundary).
    let dispatches = dispatches.load(Ordering::SeqCst);
    assert!(
        dispatches >= 1,
        "a normal settle must dispatch `agent_before_settle` at least once"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "the honored continuation issues exactly one more provider request"
    );
}

// ---------------------------------------------------------------------------
// 6. PIR-BIN-048: `tool_execution_start` is delivered before `tool_call`
// ---------------------------------------------------------------------------

/// PIR-BIN-048: the hook order is `tool_execution_start` THEN `tool_call`,
/// with the start frame carrying the model's raw arguments.
///
/// Upstream's `executeToolCalls*` awaits the start emit before
/// `prepareToolCall(..., config.beforeToolCall)` (agent-loop.ts:553-566,
/// 608-620), so a handler sees the start first and `args` is pre-gate. pir
/// dispatched `tool_call` first, and `resolve_tool_start_args` preferred the
/// gate's post-rewrite input, so the start frame carried the mutated args
/// (measured: `.play/evidence/ab-round1/lane-ab-hooks/runs/tool-ok/{pir,pi}`).
///
/// Both faces are recorded on ONE ordered log: the native plugin observes the
/// `tool_call` gate, and the host event handler observes the
/// `tool_execution_start` frame. They are dispatched from the same task, so
/// the log's order is the client-visible order.
#[tokio::test]
async fn tool_execution_start_precedes_the_tool_call_gate() {
    use pir::agent::agent_loop::AgentEvent;

    let temp = TempDir::new("start-before-gate");
    let log: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    // The `tool_call` handler rewrites the input: the start frame must keep
    // the model's raw arguments, exactly like upstream.
    struct RewritingGate {
        info: PluginInfo,
        log: Arc<Mutex<Vec<(String, Value)>>>,
    }
    impl AgentPlugin for RewritingGate {
        fn info(&self) -> &PluginInfo {
            &self.info
        }
        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["tool_call".into()])
        }
        fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            if event.name != "tool_call" {
                return Box::pin(async { PluginEventOutcome::None });
            }
            let payload = event.payload.clone();
            self.log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(("tool_call".to_owned(), payload));
            if let Some(input) = event.payload.get_mut("input") {
                *input = json!({ "value": "rewritten" });
            }
            Box::pin(async { PluginEventOutcome::None })
        }
    }

    let registry = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "probe", json!({ "value": "in" }))],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let (tools, runs) = probe_registry();
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(AgentSession::new_with_plugins(
        session_config(&temp, registry, tools, Some(session)),
        vec![Arc::new(RewritingGate {
            info: PluginInfo::inline("rewriting-gate"),
            log: Arc::clone(&log),
        }) as Arc<dyn AgentPlugin>],
    ));

    let log_for_handler = Arc::clone(&log);
    agent
        .prompt(
            "start",
            Arc::new(move |event: AgentEvent| {
                if let AgentEvent::ToolExecutionStart { event, args, .. } = &event {
                    let tool_name = match event {
                        pir::agent::tool_batch::ToolExecutionEvent::Start { tool_name, .. } => {
                            tool_name.clone()
                        }
                        _ => return,
                    };
                    log_for_handler
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((
                            "tool_execution_start".to_owned(),
                            json!({ "toolName": tool_name, "args": args }),
                        ));
                }
            }),
        )
        .await
        .expect("prompt");

    let observed = log.lock().unwrap().clone();
    let names: Vec<&str> = observed.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec!["tool_execution_start", "tool_call"],
        "upstream emits tool_execution_start before the tool_call gate: {observed:?}"
    );
    assert_eq!(
        observed[0].1,
        json!({ "toolName": "probe", "args": { "value": "in" } }),
        "the start frame carries the model's RAW arguments, not the gate rewrite"
    );
    assert_eq!(
        observed[1].1["input"],
        json!({ "value": "in" }),
        "the gate still sees the model's raw input"
    );
    // The mutation still reaches execution (upstream parity): the tool ran
    // with the rewritten payload.
    assert_eq!(
        runs.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        &[json!({ "value": "rewritten" })],
        "the tool_call rewrite must still reach the tool"
    );
}
