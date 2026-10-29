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
