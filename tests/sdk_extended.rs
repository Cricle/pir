//! Extended Rust SDK surface: image prompts, session lifecycle, and
//! introspection.
//!
//! Every test is hermetic: the model is a scripted in-process
//! [`Provider`] (no network, no `~/.pi` configuration), and session files are
//! written under per-test temp directories. The scripted provider records the
//! [`Context`] of every request, so the tests assert on what the model
//! actually receives rather than on internal bookkeeping.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde_json::json;

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{
    AfterToolCallResult, AgentTool, BeforeToolCallResult, BoxFuture, ToolContext, ToolHooks,
};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, ErrorReason, ImageContent,
    Message, Model, ModelCost, ModelCostRates, StopReason, StreamOptions, TerminalReason,
    TextContent, ThinkingLevel, Usage, UserMessage, UserRole,
};
use pir::plugins::{
    AgentPlugin, CommandBackend, CommandHandler, EventInterest, PluginEvent, PluginEventOutcome,
    PluginFuture, PluginInfo, RegistrationContext,
};
use pir::sdk::{
    AgentBuilder, CreateAgentSessionOptions, DefaultResourceLoader, ModelRegistry, SdkError,
    SdkEvent, SessionMode, SystemPromptFiles,
};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Scripted provider
// ---------------------------------------------------------------------------

/// Responder signature: (call index, request context) -> assistant message.
type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

struct ScriptedProvider {
    calls: AtomicUsize,
    respond: Responder,
}

fn mock_model() -> Model {
    Model {
        id: "scripted-model".into(),
        name: "Scripted model".into(),
        api: "scripted-api".into(),
        provider: "scripted".into(),
        base_url: "http://127.0.0.1:1".into(),
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
        context_window: 8_192,
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

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "scripted".into(),
        provider: "scripted".into(),
        model: "scripted-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        usage: Usage::default(),
        ..Default::default()
    }
}

fn tool_call(id: &str, name: &str) -> ContentBlock {
    tool_call_block(id, name, Vec::new())
}

/// A tool-call content block with explicit arguments.
fn tool_call_block(
    id: &str,
    name: &str,
    arguments: Vec<(&str, serde_json::Value)>,
) -> ContentBlock {
    ContentBlock::ToolCall(pir::ai::types::ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments: arguments
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// The parsed [`ToolCall`](pir::ai::types::ToolCall) inside a content block.
fn as_tool_call(block: &ContentBlock) -> pir::ai::types::ToolCall {
    match block {
        ContentBlock::ToolCall(call) => call.clone(),
        other => panic!("expected a tool-call block, got {other:?}"),
    }
}

/// Concatenated text blocks of a content list.
fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Concatenated text content of any transcript message role.
fn message_text(message: &Message) -> String {
    match message {
        Message::User(payload) | Message::System(payload) | Message::Developer(payload) => {
            content_text(&payload.content)
        }
        Message::Assistant(payload) => content_text(&payload.content),
        Message::ToolResult(payload) => content_text(&payload.content),
    }
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "scripted"
    }

    fn name(&self) -> &str {
        "Scripted provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![mock_model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        // `with_registry` streams unchecked, so no key source is required.
        Vec::new()
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
        if let Some(ContentBlock::Text(text)) = message
            .content
            .iter()
            .find(|block| matches!(block, ContentBlock::Text(_)))
        {
            stream.push(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: text.text.clone(),
                partial: Arc::new(message.clone()),
            });
        }
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn registry(respond: Responder) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(ScriptedProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

/// A provider that answers each model request with explicit streaming frames,
/// so a test can drive tool-call deltas and terminal `done`/`error` frames.
struct FrameProvider {
    calls: AtomicUsize,
    frames: Arc<dyn Fn(usize) -> Vec<AssistantMessageEvent> + Send + Sync>,
}

impl Provider for FrameProvider {
    fn id(&self) -> &str {
        "scripted"
    }

    fn name(&self) -> &str {
        "Framed provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![mock_model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        // `with_registry` streams unchecked, so no key source is required.
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        for event in (self.frames)(index) {
            stream.push(event);
        }
        stream
    }
}

fn frame_registry(
    frames: Arc<dyn Fn(usize) -> Vec<AssistantMessageEvent> + Send + Sync>,
) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FrameProvider {
        calls: AtomicUsize::new(0),
        frames,
    }));
    Arc::new(registry)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Per-test temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-sdk-extended-{label}-{}-{}",
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

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Serializes tests that read `PIR_SYSTEM_PROMPT_FILE`: the variable is
/// process-wide while cargo runs tests in parallel by default.
static PROMPT_ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock_prompt_env() -> std::sync::MutexGuard<'static, ()> {
    PROMPT_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sets/removes `PIR_SYSTEM_PROMPT_FILE` for one test and restores it.
struct PromptEnvGuard {
    previous: Option<std::ffi::OsString>,
}

impl PromptEnvGuard {
    fn set(value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os("PIR_SYSTEM_PROMPT_FILE");
        std::env::set_var("PIR_SYSTEM_PROMPT_FILE", value);
        Self { previous }
    }

    fn remove() -> Self {
        let previous = std::env::var_os("PIR_SYSTEM_PROMPT_FILE");
        std::env::remove_var("PIR_SYSTEM_PROMPT_FILE");
        Self { previous }
    }
}

impl Drop for PromptEnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("PIR_SYSTEM_PROMPT_FILE", value),
            None => std::env::remove_var("PIR_SYSTEM_PROMPT_FILE"),
        }
    }
}

fn agent_builder(registry: Arc<ProviderRegistry>, cwd: &Path) -> AgentBuilder {
    AgentBuilder::new()
        .with_registry(registry, mock_model())
        .cwd(cwd)
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
}

/// The content blocks of the last user message in `context`.
fn last_user_content(context: &Context) -> &[ContentBlock] {
    context
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User(user) => Some(user.content.as_slice()),
            _ => None,
        })
        .unwrap_or_default()
}

/// A tool whose run blocks until the gate opens or the context aborts.
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

/// A custom tool registered through `with_tool`, used for introspection.
struct WordCountTool;

impl AgentTool for WordCountTool {
    fn name(&self) -> &'static str {
        "word_count"
    }

    fn description(&self) -> &'static str {
        "Count the words in a text"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }))
    }

    fn run(&self, args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let count = args["text"]
            .as_str()
            .map(|value| value.split_whitespace().count())
            .unwrap_or(0);
        Box::pin(async move { ToolResult::ok(format!("{count} words")) })
    }
}

async fn wait_for_tool_entry(entered: &Arc<AtomicUsize>) {
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("tool never started");
}

// ---------------------------------------------------------------------------
// Image prompts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prompt_with_images_sends_text_then_image_to_provider() {
    let cwd = TempDir::new("images-cwd");
    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |_index, context| {
        recorded.lock().unwrap().push(context.clone());
        assistant(vec![text_block("image ok")], StopReason::Stop)
    }));
    let agent = agent_builder(registry, cwd.path())
        .no_session()
        .build()
        .await
        .expect("build agent");

    let image = ImageContent {
        data: "aGVsbG8gaW1hZ2U=".to_owned(),
        mime_type: "image/png".to_owned(),
    };
    let outcome = agent
        .prompt_with_images("describe this", vec![image.clone()])
        .await
        .expect("prompt with images");
    assert_eq!(outcome.message_text().as_deref(), Some("image ok"));

    let context = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("the provider saw one request");
    let content = last_user_content(&context);
    assert_eq!(
        content.len(),
        2,
        "the starting user message is [text, image]: {content:?}"
    );
    match &content[0] {
        ContentBlock::Text(text) => assert_eq!(text.text, "describe this"),
        other => panic!("expected the text block first, got {other:?}"),
    }
    match &content[1] {
        ContentBlock::Image(received) => assert_eq!(received, &image),
        other => panic!("expected the image block second, got {other:?}"),
    }

    // The streaming variant takes the same typed images argument and puts the
    // image on the same starting message.
    let events: Vec<SdkEvent> = agent
        .prompt_stream_with_images("stream this", vec![image.clone()])
        .collect()
        .await;
    assert!(matches!(
        events.last(),
        Some(SdkEvent::AgentSettled { aborted: false })
    ));
    let contexts = seen.lock().unwrap();
    let content = last_user_content(contexts.last().expect("second request"));
    assert_eq!(content.len(), 2);
    match &content[1] {
        ContentBlock::Image(received) => assert_eq!(received, &image),
        other => panic!("expected the image block second, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// SDK event projection (H6): the full AgentEvent surface
// ---------------------------------------------------------------------------

/// A framed run that calls `word_count`, so every projection below shares the
/// same shape: one tool-call turn (with incremental tool-call frames) and one
/// final text turn.
fn word_count_frames(index: usize) -> Vec<AssistantMessageEvent> {
    if index == 0 {
        let start_partial = assistant(
            vec![tool_call_block("call-1", "word_count", Vec::new())],
            StopReason::ToolUse,
        );
        let complete = assistant(
            vec![tool_call_block(
                "call-1",
                "word_count",
                vec![("text", serde_json::json!("one two"))],
            )],
            StopReason::ToolUse,
        );
        let tool_call = as_tool_call(&complete.content[0]);
        vec![
            AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: Arc::new(start_partial),
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"text\":".to_owned(),
                partial: Arc::new(complete.clone()),
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "\"one two\"}".to_owned(),
                partial: Arc::new(complete.clone()),
            },
            AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call,
                partial: Arc::new(complete.clone()),
            },
            AssistantMessageEvent::Done {
                reason: TerminalReason::ToolUse,
                message: complete,
            },
        ]
    } else {
        let message = assistant(vec![text_block("done")], StopReason::Stop);
        vec![
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "done".to_owned(),
                partial: Arc::new(message.clone()),
            },
            AssistantMessageEvent::Done {
                reason: TerminalReason::Stop,
                message,
            },
        ]
    }
}

fn terminal_reason_wire(reason: TerminalReason) -> &'static str {
    match reason {
        TerminalReason::Stop => "stop",
        TerminalReason::Length => "length",
        TerminalReason::ToolUse => "toolUse",
        TerminalReason::Deferred => "deferred",
    }
}

fn error_reason_wire(reason: ErrorReason) -> &'static str {
    match reason {
        ErrorReason::Error => "error",
        ErrorReason::Aborted => "aborted",
    }
}

/// The SDK projection of one assistant stream frame, in the same vocabulary
/// as the extension `message_update.assistantMessageEvent.type` wire strings.
fn sdk_frame_signature(event: &SdkEvent) -> Option<String> {
    match event {
        SdkEvent::MessageUpdate { delta, thinking } => Some(format!(
            "{}:{delta}",
            if *thinking {
                "thinking_delta"
            } else {
                "text_delta"
            }
        )),
        SdkEvent::ToolCallStart { content_index, .. } => {
            Some(format!("toolcall_start:{content_index}"))
        }
        SdkEvent::ToolCallDelta {
            content_index,
            delta,
            ..
        } => Some(format!("toolcall_delta:{content_index}:{delta}")),
        SdkEvent::ToolCallEnd {
            content_index,
            tool_call,
            ..
        } => Some(format!("toolcall_end:{content_index}:{}", tool_call.id)),
        SdkEvent::MessageDone { reason, .. } => {
            Some(format!("done:{}", terminal_reason_wire(*reason)))
        }
        SdkEvent::MessageError { reason, .. } => {
            Some(format!("error:{}", error_reason_wire(*reason)))
        }
        _ => None,
    }
}

/// The same signature derived from one native-plugin `message_update` payload
/// (the extension face's `assistantMessageEvent`).
fn extension_frame_signature(payload: &serde_json::Value) -> String {
    let frame = &payload["assistantMessageEvent"];
    match frame["type"].as_str().unwrap_or_default() {
        "text_delta" | "thinking_delta" => format!(
            "{}:{}",
            frame["type"].as_str().unwrap_or_default(),
            frame["delta"].as_str().unwrap_or_default()
        ),
        "toolcall_start" => format!("toolcall_start:{}", frame["content_index"]),
        "toolcall_delta" => format!(
            "toolcall_delta:{}:{}",
            frame["content_index"],
            frame["delta"].as_str().unwrap_or_default()
        ),
        "toolcall_end" => format!(
            "toolcall_end:{}:{}",
            frame["content_index"],
            frame["tool_call"]["id"].as_str().unwrap_or_default()
        ),
        "done" => format!("done:{}", frame["reason"].as_str().unwrap_or_default()),
        "error" => format!("error:{}", frame["reason"].as_str().unwrap_or_default()),
        other => format!("other:{other}"),
    }
}

/// The session-event → stream bridge is installed by the first STREAMING run
/// (not at build), so a non-streaming agent never holds a permanent
/// session-event subscriber. This guards the install point: a stream started
/// after `Agent::prompt` runs must still observe session-level events.
#[tokio::test(flavor = "current_thread")]
async fn first_prompt_stream_after_non_streaming_runs_receives_session_events() {
    let cwd = TempDir::new("lazy-stream-bridge");
    let registry = registry(Arc::new(|_index, _context| {
        assistant(vec![text_block("ok")], StopReason::Stop)
    }));
    let agent = agent_builder(registry, cwd.path())
        .agent_dir(cwd.path())
        .session(SessionMode::Dir(cwd.path().join("sessions")))
        .build()
        .await
        .expect("build agent");

    agent.prompt("warmup").await.expect("warmup prompt");

    let events: Vec<SdkEvent> = agent.prompt_stream("stream").collect().await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            SdkEvent::EntryAppended { entry }
                if entry
                    .get("message")
                    .and_then(|message| message.get("role"))
                    .and_then(|role| role.as_str())
                    == Some("assistant")
        )),
        "the first stream after non-streaming runs must still receive \
         session-level events: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(SdkEvent::AgentSettled { .. })),
        "the stream must still end with its terminal event: {events:?}"
    );
}

#[tokio::test]
async fn sdk_projects_non_assistant_message_lifecycle() {
    let cwd = TempDir::new("projection-lifecycle");
    let registry = frame_registry(Arc::new(|index| {
        if index == 0 {
            let message = assistant(
                vec![tool_call_block(
                    "call-1",
                    "word_count",
                    vec![("text", serde_json::json!("one two"))],
                )],
                StopReason::ToolUse,
            );
            vec![AssistantMessageEvent::Done {
                reason: TerminalReason::ToolUse,
                message,
            }]
        } else {
            let message = assistant(vec![text_block("done")], StopReason::Stop);
            vec![
                AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "done".to_owned(),
                    partial: Arc::new(message.clone()),
                },
                AssistantMessageEvent::Done {
                    reason: TerminalReason::Stop,
                    message,
                },
            ]
        }
    }));
    let agent = agent_builder(registry, cwd.path())
        .no_session()
        .with_tool(Arc::new(WordCountTool))
        .build()
        .await
        .expect("build agent");

    let events: Vec<SdkEvent> = agent.prompt_stream("count the words").collect().await;

    let starts: Vec<&Message> = events
        .iter()
        .filter_map(|event| match event {
            SdkEvent::TranscriptMessageStart { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    let ends: Vec<&Message> = events
        .iter()
        .filter_map(|event| match event {
            SdkEvent::TranscriptMessageEnd { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(
        starts.len(),
        ends.len(),
        "every non-assistant message lifecycle is closed: {starts:?} / {ends:?}"
    );

    let user = starts
        .iter()
        .find(|message| matches!(message, Message::User(_)))
        .expect("the starting user message lifecycle is visible");
    assert_eq!(message_text(user), "count the words");
    assert!(
        ends.contains(user),
        "the user message end carries the same message"
    );

    let tool_result = starts
        .iter()
        .find(|message| matches!(message, Message::ToolResult(_)))
        .expect("the tool-result message lifecycle is visible");
    match tool_result {
        Message::ToolResult(result) => {
            assert_eq!(result.tool_call_id, "call-1");
            assert_eq!(result.tool_name, "word_count");
            assert!(!result.is_error);
            assert_eq!(content_text(&result.content), "2 words");
        }
        other => panic!("expected a tool-result message, got {other:?}"),
    }
    assert!(
        ends.iter().any(|message| matches!(
            message,
            Message::ToolResult(result) if result.tool_call_id == "call-1"
        )),
        "the tool-result end is projected"
    );
}

#[tokio::test]
async fn sdk_projects_toolcall_stream_frames_in_order_and_arguments() {
    let cwd = TempDir::new("projection-toolcall");
    let registry = frame_registry(Arc::new(word_count_frames));
    let agent = agent_builder(registry, cwd.path())
        .no_session()
        .with_tool(Arc::new(WordCountTool))
        .build()
        .await
        .expect("build agent");

    let events: Vec<SdkEvent> = agent
        .prompt_stream_with_outcome("stream the call")
        .collect()
        .await;

    let mut signatures: Vec<String> = Vec::new();
    let mut deltas: Vec<String> = Vec::new();
    let mut parsed_tool_call = None;
    for event in &events {
        match event {
            SdkEvent::ToolCallStart {
                turn,
                content_index,
            } => {
                assert_eq!(*turn, 1, "the first turn owns the tool-call frames");
                signatures.push(format!("start:{content_index}"));
            }
            SdkEvent::ToolCallDelta {
                turn,
                content_index,
                delta,
            } => {
                assert_eq!(*turn, 1);
                assert_eq!(*content_index, 0);
                signatures.push(format!("delta:{delta}"));
                deltas.push(delta.clone());
            }
            SdkEvent::ToolCallEnd {
                turn,
                content_index,
                tool_call,
            } => {
                assert_eq!(*turn, 1);
                assert_eq!(*content_index, 0);
                signatures.push(format!("end:{}", tool_call.id));
                parsed_tool_call = Some(tool_call.clone());
            }
            _ => {}
        }
    }
    assert_eq!(
        signatures,
        vec![
            "start:0",
            "delta:{\"text\":",
            "delta:\"one two\"}",
            "end:call-1"
        ],
        "tool-call frames arrive in provider order"
    );
    assert_eq!(
        deltas.concat(),
        "{\"text\":\"one two\"}",
        "the deltas reconstruct the provider's argument JSON"
    );
    let tool_call = parsed_tool_call.expect("ToolCallEnd carries the parsed call");
    assert_eq!(tool_call.name, "word_count");
    assert_eq!(
        tool_call.arguments.get("text"),
        Some(&serde_json::json!("one two"))
    );

    // The wrapped outcome proves the parsed call actually executed.
    let outcome = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::AgentOutcome(outcome) => Some(outcome),
            _ => None,
        })
        .expect("prompt_stream_with_outcome carries the outcome");
    assert_eq!(outcome.tool_result_texts(), vec!["2 words".to_owned()]);

    // Both public payloads stay live on this path: the per-run `AgentEnd`
    // event and the terminal outcome carry the same produced-message slice.
    let agent_end_messages = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::AgentEnd { messages, .. } => Some(messages),
            _ => None,
        })
        .expect("prompt_stream_with_outcome still carries agent_end");
    assert_eq!(
        agent_end_messages, &outcome.messages,
        "agent_end.messages and AgentOutcome.messages must describe the same run"
    );
}

#[tokio::test]
async fn sdk_projects_message_done_error_and_agent_end() {
    let cwd = TempDir::new("projection-terminal");

    // Successful terminal frame + per-run agent_end.
    let registry = frame_registry(Arc::new(|_index| {
        let message = assistant(vec![text_block("complete")], StopReason::Stop);
        vec![
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "complete".to_owned(),
                partial: Arc::new(message.clone()),
            },
            AssistantMessageEvent::Done {
                reason: TerminalReason::Stop,
                message,
            },
        ]
    }));
    let agent = agent_builder(registry, cwd.path())
        .no_session()
        .build()
        .await
        .expect("build agent");
    let events: Vec<SdkEvent> = agent.prompt_stream("finish").collect().await;

    let (reason, message) = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::MessageDone {
                reason, message, ..
            } => Some((*reason, message)),
            _ => None,
        })
        .expect("the done frame is projected");
    assert_eq!(reason, TerminalReason::Stop);
    assert_eq!(content_text(&message.content), "complete");

    let end = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::AgentEnd {
                messages,
                will_retry,
            } => Some((messages, *will_retry)),
            _ => None,
        })
        .expect("agent_end is projected");
    assert!(!end.1, "a successful run has no retry pending");
    assert!(
        end.0
            .iter()
            .any(|message| message_text(message) == "complete"),
        "agent_end carries the run's produced messages: {:?}",
        end.0
    );
    assert!(
        matches!(events.last(), Some(SdkEvent::AgentSettled { .. })),
        "agent_end is not the stream's terminal event"
    );

    // Failed terminal frame.
    let error_registry = frame_registry(Arc::new(|_index| {
        let mut failed = assistant(vec![text_block("partial")], StopReason::Error);
        failed.error_message = Some("provider exploded".to_owned());
        let partial = assistant(vec![text_block("partial")], StopReason::Stop);
        vec![
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "partial".to_owned(),
                partial: Arc::new(partial),
            },
            AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error: failed,
            },
        ]
    }));
    let failing = agent_builder(error_registry, cwd.path())
        .no_session()
        .build()
        .await
        .expect("build failing agent");
    let events: Vec<SdkEvent> = failing.prompt_stream("fail").collect().await;

    let (reason, message) = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::MessageError {
                reason, message, ..
            } => Some((*reason, message)),
            _ => None,
        })
        .expect("the error frame is projected");
    assert_eq!(reason, ErrorReason::Error);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("provider exploded"),
        "the projected error message keeps the provider's error text"
    );
    let end = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::MessageEnd { message } => Some(message),
            _ => None,
        })
        .expect("the errored message still ends the stream lifecycle");
    assert_eq!(end.stop_reason, StopReason::Error);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SdkEvent::AgentEnd { .. })),
        "an errored run still emits agent_end"
    );
    assert!(matches!(events.last(), Some(SdkEvent::AgentSettled { .. })));
}

#[tokio::test]
async fn sdk_projection_matches_extension_events_for_the_same_run() {
    let cwd = TempDir::new("projection-consistency");
    let registry = frame_registry(Arc::new(word_count_frames));

    let extension_events: Arc<Mutex<Vec<(&'static str, serde_json::Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let mut builder = agent_builder(registry, cwd.path())
        .no_session()
        .with_tool(Arc::new(WordCountTool));
    for name in [
        "message_start",
        "message_update",
        "message_end",
        "agent_end",
    ] {
        let log = Arc::clone(&extension_events);
        builder = builder.on_event(name, move |payload| {
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((name, payload.clone()));
        });
    }
    let agent = builder.build().await.expect("build agent");
    let events: Vec<SdkEvent> = agent.prompt_stream("compare").collect().await;

    let extension_events = extension_events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();

    // 1. Assistant stream frames: every extension `message_update` frame the
    // SDK projects maps to exactly the same SDK event, in the same order.
    let extension_frames: Vec<String> = extension_events
        .iter()
        .filter(|(name, _)| *name == "message_update")
        .map(|(_, payload)| extension_frame_signature(payload))
        .collect();
    let sdk_frames: Vec<String> = events.iter().filter_map(sdk_frame_signature).collect();
    assert_eq!(
        sdk_frames, extension_frames,
        "the SDK frame projection must match the extension message_update frames"
    );

    // 2. Message lifecycle roles: assistant stream starts/ends plus every
    // non-assistant transcript message the extension face announces.
    let mut extension_start_roles: Vec<&str> = extension_events
        .iter()
        .filter(|(name, _)| *name == "message_start")
        .map(|(_, payload)| payload["message"]["role"].as_str().unwrap_or_default())
        .collect();
    let mut extension_end_roles: Vec<&str> = extension_events
        .iter()
        .filter(|(name, _)| *name == "message_end")
        .map(|(_, payload)| payload["message"]["role"].as_str().unwrap_or_default())
        .collect();
    let mut sdk_start_roles: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            SdkEvent::MessageStart => Some("assistant"),
            SdkEvent::TranscriptMessageStart { message, .. } => Some(message.role()),
            _ => None,
        })
        .collect();
    let mut sdk_end_roles: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            SdkEvent::MessageEnd { .. } => Some("assistant"),
            SdkEvent::TranscriptMessageEnd { message, .. } => Some(message.role()),
            _ => None,
        })
        .collect();
    extension_start_roles.sort_unstable();
    sdk_start_roles.sort_unstable();
    extension_end_roles.sort_unstable();
    sdk_end_roles.sort_unstable();
    assert_eq!(
        sdk_start_roles, extension_start_roles,
        "every extension message_start has one SDK start projection"
    );
    assert_eq!(
        sdk_end_roles, extension_end_roles,
        "every extension message_end has one SDK end projection"
    );

    // 3. agent_end: the SDK event carries the same produced messages.
    let extension_agent_end = extension_events
        .iter()
        .find(|(name, _)| *name == "agent_end")
        .map(|(_, payload)| payload)
        .expect("the extension agent_end fired");
    let extension_messages: Vec<(String, String)> = extension_agent_end["messages"]
        .as_array()
        .expect("agent_end.messages is an array")
        .iter()
        .map(|message| {
            let role = message["role"].as_str().unwrap_or_default().to_owned();
            let text = message["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|block| block["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            (role, text)
        })
        .collect();
    let sdk_agent_end = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::AgentEnd { messages, .. } => Some(messages),
            _ => None,
        })
        .expect("the SDK agent_end is projected");
    let sdk_messages: Vec<(String, String)> = sdk_agent_end
        .iter()
        .map(|message| (message.role().to_owned(), message_text(message)))
        .collect();
    assert_eq!(
        sdk_messages, extension_messages,
        "agent_end.messages matches the extension projection"
    );
}

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn new_session_replaces_session_id_and_path_and_writes_the_new_file() {
    let dir = TempDir::new("new-session");
    let registry = registry(Arc::new(|_index, _context| {
        assistant(vec![text_block("ok")], StopReason::Stop)
    }));
    let agent = agent_builder(registry, dir.path())
        .session(SessionMode::Dir(dir.path().to_path_buf()))
        .build()
        .await
        .expect("build agent");

    agent.prompt("first prompt").await.expect("first prompt");
    let first_path = agent.session_path().expect("first session path");
    let first_id = agent.session_id().expect("first session id");
    assert!(first_path.exists(), "the first prompt persisted the file");

    agent.new_session().await.expect("new session");
    let second_path = agent.session_path().expect("second session path");
    let second_id = agent.session_id().expect("second session id");
    assert_ne!(first_path, second_path, "new_session picks a new file");
    assert_ne!(first_id, second_id, "new_session picks a new id");
    assert!(
        !second_path.exists(),
        "a fresh session file is created lazily by the first message"
    );

    agent.prompt("second prompt").await.expect("second prompt");
    assert!(second_path.exists(), "the second prompt wrote the new file");
    let second_content = std::fs::read_to_string(&second_path).expect("read new session");
    assert!(second_content.contains("second prompt"), "{second_content}");
    assert!(!second_content.contains("first prompt"), "{second_content}");

    let first_content = std::fs::read_to_string(&first_path).expect("read old session");
    assert!(first_content.contains("first prompt"), "{first_content}");
    assert!(!first_content.contains("second prompt"), "{first_content}");
}

#[tokio::test]
async fn attach_session_replays_the_existing_transcript_before_the_next_prompt() {
    let source_dir = TempDir::new("attach-source");
    let target_dir = TempDir::new("attach-target");

    // Produce a real session file with one completed turn.
    let source_registry = registry(Arc::new(|_index, _context| {
        assistant(vec![text_block("alpha reply")], StopReason::Stop)
    }));
    let source = agent_builder(source_registry, source_dir.path())
        .session(SessionMode::Dir(source_dir.path().to_path_buf()))
        .build()
        .await
        .expect("build source agent");
    source
        .prompt("alpha question")
        .await
        .expect("source prompt");
    let source_path = source.session_path().expect("source session path");
    let source_id = source.session_id().expect("source session id");
    source.shutdown().await.expect("source shutdown");
    drop(source);

    // Attach it to an agent backed by a different session directory, then
    // ask a follow-up: the provider must see the replayed transcript.
    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |_index, context| {
        recorded.lock().unwrap().push(context.clone());
        assistant(vec![text_block("beta reply")], StopReason::Stop)
    }));
    let agent = agent_builder(registry, target_dir.path())
        .session(SessionMode::Dir(target_dir.path().to_path_buf()))
        .build()
        .await
        .expect("build target agent");

    agent
        .attach_session(&source_path)
        .await
        .expect("attach session");
    assert_eq!(
        std::fs::canonicalize(agent.session_path().expect("attached path")).unwrap(),
        std::fs::canonicalize(&source_path).unwrap(),
        "attach_session switches the live session file"
    );
    assert_eq!(agent.session_id(), Some(source_id));

    agent
        .prompt("beta question")
        .await
        .expect("follow-up prompt");
    let contexts = seen.lock().unwrap();
    let context = contexts.last().expect("follow-up request");
    let user_texts: Vec<String> = context
        .messages
        .iter()
        .filter_map(|message| match message {
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
            _ => None,
        })
        .collect();
    assert!(
        user_texts.iter().any(|text| text == "alpha question"),
        "the attached transcript was replayed: {user_texts:?}"
    );
    assert!(
        context.messages.iter().any(|message| matches!(
            message,
            Message::Assistant(assistant)
                if assistant
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text(text) if text.text == "alpha reply"))
        )),
        "the prior assistant reply is part of the replayed context"
    );
    assert_eq!(
        user_texts.last().map(String::as_str),
        Some("beta question"),
        "the new prompt follows the replayed transcript"
    );
}

#[tokio::test]
async fn attach_session_is_rejected_while_a_run_is_in_flight() {
    let source_dir = TempDir::new("attach-busy-source");
    let busy_dir = TempDir::new("attach-busy-target");

    let source_registry = registry(Arc::new(|_index, _context| {
        assistant(vec![text_block("seed reply")], StopReason::Stop)
    }));
    let source = agent_builder(source_registry, source_dir.path())
        .session(SessionMode::Dir(source_dir.path().to_path_buf()))
        .build()
        .await
        .expect("build source agent");
    source.prompt("seed question").await.expect("seed prompt");
    let source_path = source.session_path().expect("source session path");
    source.shutdown().await.expect("source shutdown");
    drop(source);

    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
        } else {
            assistant(vec![text_block("done")], StopReason::Stop)
        }
    }));
    let agent = agent_builder(registry, busy_dir.path())
        .session(SessionMode::Dir(busy_dir.path().to_path_buf()))
        .with_tool(Arc::new(GateTool {
            gate: gate_rx,
            entered: Arc::clone(&entered),
        }))
        .build()
        .await
        .expect("build busy agent");

    let prompt_task = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("start").await }
    });
    wait_for_tool_entry(&entered).await;

    let error = agent
        .attach_session(&source_path)
        .await
        .expect_err("attach while streaming must be rejected");
    assert!(
        error.to_string().contains("in flight"),
        "the rejection names the in-flight run: {error}"
    );

    gate_tx.send(true).expect("release the gate");
    let outcome = prompt_task.await.expect("join").expect("prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("done"));
}

// ---------------------------------------------------------------------------
// Introspection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tools_lists_a_custom_tool_registered_with_with_tool() {
    let cwd = TempDir::new("tools-cwd");
    let registry = registry(Arc::new(|_index, _context| {
        assistant(vec![text_block("ok")], StopReason::Stop)
    }));
    let agent = agent_builder(registry, cwd.path())
        .no_session()
        .with_tool(Arc::new(WordCountTool))
        .build()
        .await
        .expect("build agent");

    let tools = agent.tools();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(
        names.contains(&"word_count"),
        "the custom tool is in the merged registry: {names:?}"
    );

    let custom = tools
        .iter()
        .find(|tool| tool.name == "word_count")
        .expect("word_count metadata");
    assert_eq!(custom.description, "Count the words in a text");
    assert_eq!(
        custom
            .parameters
            .as_ref()
            .and_then(|schema| schema["type"].as_str()),
        Some("object"),
        "the declared parameter schema is exposed"
    );

    // The registry snapshot is sorted for deterministic iteration.
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);

    // No JS extension runtime: the notification queue is empty, never an
    // error.
    assert!(agent.take_notifications().is_empty());
}

#[tokio::test]
async fn is_idle_and_pending_message_count_track_a_scripted_run() {
    let cwd = TempDir::new("pending-cwd");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
        } else {
            assistant(vec![text_block("done")], StopReason::Stop)
        }
    }));
    let agent = agent_builder(registry, cwd.path())
        .no_session()
        .with_tool(Arc::new(GateTool {
            gate: gate_rx,
            entered: Arc::clone(&entered),
        }))
        .build()
        .await
        .expect("build agent");

    assert!(agent.is_idle());
    assert_eq!(agent.pending_message_count(), 0);

    let prompt_task = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("start").await }
    });
    wait_for_tool_entry(&entered).await;
    assert!(!agent.is_idle(), "the tool call keeps the run in flight");
    assert_eq!(agent.pending_message_count(), 0);

    agent.steer("steer while busy");
    assert_eq!(
        agent.pending_message_count(),
        1,
        "a steering message queued during a run is pending"
    );

    gate_tx.send(true).expect("release the gate");
    let outcome = prompt_task.await.expect("join").expect("prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("done"));
    assert!(agent.is_idle(), "the run settled");
    assert_eq!(
        agent.pending_message_count(),
        0,
        "the steering message was delivered during the run"
    );
}

// ---------------------------------------------------------------------------
// Session name
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_session_name_is_visible_through_session_name_and_session_metadata() {
    let dir = TempDir::new("session-name");
    let registry = registry(Arc::new(|_index, _context| {
        assistant(vec![text_block("ok")], StopReason::Stop)
    }));
    let agent = agent_builder(registry, dir.path())
        .session(SessionMode::Dir(dir.path().to_path_buf()))
        .build()
        .await
        .expect("build agent");
    agent.prompt("hello").await.expect("prompt");

    let normalized = agent
        .set_session_name(Some("Release notes"))
        .await
        .expect("set session name");
    assert_eq!(normalized.as_deref(), Some("Release notes"));
    assert_eq!(agent.session_name().await.as_deref(), Some("Release notes"));

    let path = agent.session_path().expect("session path");
    let manager = SessionManager::open(&path).expect("reopen session file");
    assert_eq!(
        manager.session_name().as_deref(),
        Some("Release notes"),
        "the name is persisted as session metadata"
    );

    // Clearing removes the name from the live session and the file.
    assert_eq!(
        agent.set_session_name(None).await.expect("clear name"),
        None
    );
    assert_eq!(agent.session_name().await, None);
    let manager = SessionManager::open(&path).expect("reopen session file after clear");
    assert_eq!(manager.session_name(), None);
}

// ---------------------------------------------------------------------------
// SYSTEM.md / APPEND_SYSTEM.md discovery (print's precedence, host parity)
// ---------------------------------------------------------------------------

/// With no explicit prompt the SDK build discovers the workspace
/// SYSTEM.md / APPEND_SYSTEM.md (project files trust-gated against the
/// build's own agent dir) and applies print's precedence: a discovered
/// SYSTEM.md becomes the preamble override and APPEND_SYSTEM.md the addendum.
/// An explicit `.system_prompt(..)` pins the whole prompt, so the discovery
/// does not fire and the pinned text wins.
#[tokio::test]
async fn sdk_discovers_workspace_system_md_and_explicit_prompt_wins() {
    let _env_lock = lock_prompt_env();
    let _env = PromptEnvGuard::remove();
    let root = TempDir::new("sdk-system-md");
    let workspace = root.path().join("proj");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(workspace.join(".pi")).expect("create .pi");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    std::fs::write(workspace.join(".pi").join("SYSTEM.md"), "SDK SYSTEM MARKER")
        .expect("write SYSTEM.md");
    std::fs::write(
        workspace.join(".pi").join("APPEND_SYSTEM.md"),
        "SDK APPEND MARKER",
    )
    .expect("write APPEND_SYSTEM.md");
    // The project-level files are trust-gated: the "always" policy on the
    // build's own agent dir grants the fixture workspace.
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"defaultProjectTrust":"always"}"#,
    )
    .expect("write settings.json");

    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |_index, context| {
        recorded.lock().unwrap().push(context.clone());
        assistant(vec![text_block("ok")], StopReason::Stop)
    }));

    // Derived prompt: the discovered files join it. `with_registry` is
    // hermetic by default (D-03), so discovery is requested explicitly here.
    let agent = agent_builder(Arc::clone(&registry), &workspace)
        .agent_dir(&agent_dir)
        .discover_system_prompt(true)
        .no_session()
        .build()
        .await
        .expect("build agent");
    agent.prompt("hi").await.expect("prompt");
    let context = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("the provider saw one request");
    let system = context
        .system_prompt
        .as_deref()
        .expect("the request carries a system prompt");
    assert!(
        system.contains("SDK SYSTEM MARKER"),
        "the workspace SYSTEM.md becomes the preamble override: {system}"
    );
    assert!(
        system.contains("SDK APPEND MARKER"),
        "the workspace APPEND_SYSTEM.md renders as the addendum: {system}"
    );

    // Explicit prompt: the pinned text wins, the discovered SYSTEM.md is out.
    seen.lock().unwrap().clear();
    let pinned = agent_builder(registry, &workspace)
        .agent_dir(&agent_dir)
        .no_session()
        .system_prompt("SDK PINNED WINS")
        .build()
        .await
        .expect("build pinned agent");
    pinned.prompt("hi").await.expect("prompt");
    let context = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("the provider saw the second request");
    let system = context
        .system_prompt
        .as_deref()
        .expect("the pinned request carries a system prompt");
    assert!(
        system.contains("SDK PINNED WINS"),
        "the explicit prompt is sent as-is: {system}"
    );
    assert!(
        !system.contains("SDK SYSTEM MARKER"),
        "the discovered SYSTEM.md must not survive an explicit prompt: {system}"
    );
}

/// D-03: the explicit-model paths are environment-free. `endpoint` and
/// `with_registry` must ignore a project `.pi/SYSTEM.md`, the global agent-dir
/// `SYSTEM.md`, and `PIR_SYSTEM_PROMPT_FILE` unless the caller opts in.
#[tokio::test]
async fn endpoint_and_with_registry_ignore_ambient_system_prompts_by_default() {
    let _env_lock = lock_prompt_env();
    let root = TempDir::new("sdk-ambient-ignored");
    let workspace = root.path().join("proj");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(workspace.join(".pi")).expect("create .pi");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    std::fs::write(
        workspace.join(".pi").join("SYSTEM.md"),
        "PROJECT AMBIENT MARKER",
    )
    .expect("write project SYSTEM.md");
    std::fs::write(
        workspace.join(".pi").join("APPEND_SYSTEM.md"),
        "PROJECT APPEND MARKER",
    )
    .expect("write project APPEND_SYSTEM.md");
    std::fs::write(agent_dir.join("SYSTEM.md"), "GLOBAL AMBIENT MARKER")
        .expect("write global SYSTEM.md");
    std::fs::write(agent_dir.join("APPEND_SYSTEM.md"), "GLOBAL APPEND MARKER")
        .expect("write global APPEND_SYSTEM.md");
    // These files WOULD be discovered (trust granted) if the hermetic paths
    // silently fell back to ambient discovery.
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"defaultProjectTrust":"always"}"#,
    )
    .expect("write settings.json");
    // A directory is not readable as a string: any attempt to read the env
    // prompt file would fail the build, proving it is never opened.
    let env_prompt_dir = root.path().join("env-prompt-dir");
    std::fs::create_dir_all(&env_prompt_dir).expect("create env prompt dir");
    let _env = PromptEnvGuard::set(&env_prompt_dir);

    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |_index, context| {
        recorded.lock().unwrap().push(context.clone());
        assistant(vec![text_block("ok")], StopReason::Stop)
    }));
    let agent = agent_builder(Arc::clone(&registry), &workspace)
        .agent_dir(&agent_dir)
        .no_session()
        .build()
        .await
        .expect("hermetic with_registry build must not read prompt files");
    agent.prompt("hi").await.expect("prompt");
    let system = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .and_then(|context| context.system_prompt)
        .expect("the request carries a derived system prompt");
    for marker in [
        "PROJECT AMBIENT MARKER",
        "PROJECT APPEND MARKER",
        "GLOBAL AMBIENT MARKER",
        "GLOBAL APPEND MARKER",
    ] {
        assert!(
            !system.contains(marker),
            "with_registry must not discover ambient prompt files ({marker}): {system}"
        );
    }

    // Endpoint: same ambient files + unreadable env path, still builds.
    let endpoint = AgentBuilder::new()
        .endpoint(pir::sdk::Endpoint::new(
            "http://127.0.0.1:9/v1",
            pir::sdk::ApiKind::OpenAiCompletions,
            "endpoint-model",
        ))
        .api_key("sk-test")
        .cwd(&workspace)
        .agent_dir(&agent_dir)
        .no_session()
        .build()
        .await
        .expect("hermetic endpoint build must not read prompt files");
    assert_eq!(endpoint.model().id, "endpoint-model");
}

/// D-03: explicit `system_prompt_files(..)` replaces the ambient lookup and
/// never silently mixes in project/global files.
#[tokio::test]
async fn system_prompt_files_are_explicit_and_ambient_free() {
    let _env_lock = lock_prompt_env();
    let _env = PromptEnvGuard::remove();
    let root = TempDir::new("sdk-prompt-files");
    let workspace = root.path().join("proj");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(workspace.join(".pi")).expect("create .pi");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    std::fs::write(
        workspace.join(".pi").join("SYSTEM.md"),
        "AMBIENT SYSTEM MARKER",
    )
    .expect("write ambient SYSTEM.md");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"defaultProjectTrust":"always"}"#,
    )
    .expect("write settings.json");
    let custom_system = root.path().join("custom-system.md");
    let custom_append = root.path().join("custom-append.md");
    std::fs::write(&custom_system, "CUSTOM SYSTEM FILE MARKER").expect("write custom system");
    std::fs::write(&custom_append, "CUSTOM APPEND FILE MARKER").expect("write custom append");

    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |_index, context| {
        recorded.lock().unwrap().push(context.clone());
        assistant(vec![text_block("ok")], StopReason::Stop)
    }));
    let agent = agent_builder(Arc::clone(&registry), &workspace)
        .agent_dir(&agent_dir)
        .system_prompt_files(SystemPromptFiles {
            system: Some(custom_system),
            append: Some(custom_append),
        })
        .no_session()
        .build()
        .await
        .expect("build with explicit prompt files");
    agent.prompt("hi").await.expect("prompt");
    let system = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .and_then(|context| context.system_prompt)
        .expect("the request carries a system prompt");
    assert!(
        system.contains("CUSTOM SYSTEM FILE MARKER"),
        "the explicit system file becomes the preamble: {system}"
    );
    assert!(
        system.contains("CUSTOM APPEND FILE MARKER"),
        "the explicit append file becomes an addendum: {system}"
    );
    assert!(
        !system.contains("AMBIENT SYSTEM MARKER"),
        "explicit files must not mix in ambient discovery: {system}"
    );
}

/// D-04: the low-level `create_agent_session` factory uses the same shared
/// SYSTEM.md/APPEND_SYSTEM.md path as `AgentBuilder`. A trusted project's
/// `.pi/SYSTEM.md` applies, and an explicit `system_prompt` overrides it.
#[tokio::test]
async fn create_agent_session_discovers_trusted_project_system_md_and_explicit_prompt_wins() {
    let _env_lock = lock_prompt_env();
    let _env = PromptEnvGuard::remove();
    let root = TempDir::new("sdk-low-level-system-md");
    let workspace = root.path().join("proj");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(workspace.join(".pi")).expect("create .pi");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    std::fs::write(
        workspace.join(".pi").join("SYSTEM.md"),
        "LOW LEVEL SYSTEM MARKER",
    )
    .expect("write project SYSTEM.md");
    std::fs::write(
        workspace.join(".pi").join("APPEND_SYSTEM.md"),
        "LOW LEVEL APPEND MARKER",
    )
    .expect("write project APPEND_SYSTEM.md");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"defaultProjectTrust":"always"}"#,
    )
    .expect("write settings.json");

    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |_index, context| {
        recorded.lock().unwrap().push(context.clone());
        assistant(vec![text_block("ok")], StopReason::Stop)
    }));
    let loader = || {
        pir::sdk::DefaultResourceLoader::new(workspace.clone(), agent_dir.clone())
            .with_home(root.path().to_path_buf())
    };
    let created = pir::sdk::create_agent_session(CreateAgentSessionOptions {
        cwd: Some(workspace.clone()),
        agent_dir: Some(agent_dir.clone()),
        model_registry: Some(ModelRegistry::from_registry(Arc::clone(&registry))),
        model: Some(mock_model()),
        thinking_level: Some(ThinkingLevel::Off),
        no_session: true,
        no_tools: true,
        resource_loader: Some(loader()),
        ..Default::default()
    })
    .await
    .expect("create low-level session");
    created
        .session
        .prompt("hi", Arc::new(|_event| {}))
        .await
        .expect("prompt low-level session");
    let system = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .and_then(|context| context.system_prompt)
        .expect("the low-level request carries a system prompt");
    assert!(
        system.contains("LOW LEVEL SYSTEM MARKER"),
        "create_agent_session must discover the trusted project SYSTEM.md: {system}"
    );
    assert!(
        system.contains("LOW LEVEL APPEND MARKER"),
        "create_agent_session must discover the trusted project APPEND_SYSTEM.md: {system}"
    );

    // Explicit prompt wins and the factory performs no prompt-file discovery.
    seen.lock().unwrap().clear();
    let created = pir::sdk::create_agent_session(CreateAgentSessionOptions {
        cwd: Some(workspace.clone()),
        agent_dir: Some(agent_dir.clone()),
        model_registry: Some(ModelRegistry::from_registry(registry)),
        model: Some(mock_model()),
        thinking_level: Some(ThinkingLevel::Off),
        no_session: true,
        no_tools: true,
        system_prompt: Some("LOW LEVEL PINNED WINS".to_owned()),
        resource_loader: Some(loader()),
        ..Default::default()
    })
    .await
    .expect("create pinned low-level session");
    created
        .session
        .prompt("hi", Arc::new(|_event| {}))
        .await
        .expect("prompt pinned low-level session");
    let system = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .and_then(|context| context.system_prompt)
        .expect("the pinned low-level request carries a system prompt");
    assert!(
        system.contains("LOW LEVEL PINNED WINS"),
        "the explicit prompt is sent as-is: {system}"
    );
    assert!(
        !system.contains("LOW LEVEL SYSTEM MARKER") && !system.contains("LOW LEVEL APPEND MARKER"),
        "explicit prompt must override every discovered file: {system}"
    );
}

// PIR-SDK-002 regression (plugin-only sessions push the full session-context
// snapshot) is canonically covered by `tests/suites/sdk.rs`, compiled as
// `g_agent::sdk::plugin_only_session_pushes_the_full_session_context_snapshot`
// in the zz_merged_6 harness. It is intentionally not duplicated here.

/// An abort that lands while every call of one parallel tool batch is already
/// in flight must: settle the run as aborted, persist the assistant message
/// with both tool calls plus one error result per call, and leave the session
/// usable for the next prompt. The two `gate` calls come from ONE assistant
/// message, so a sequential scheduler can never reach `entered == 2` before
/// the abort.
#[tokio::test]
async fn abort_mid_parallel_tool_batch_persists_errors_and_recovers() {
    let temp = TempDir::new("parallel-abort");
    let (_gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "gate"), tool_call("call-2", "gate")],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text_block("recovered")], StopReason::Stop)
        }
    }));
    let gate_tool: Arc<dyn AgentTool> = Arc::new(GateTool {
        gate: gate_rx,
        entered: Arc::clone(&entered),
    });
    let agent = AgentBuilder::new()
        .with_registry(registry, mock_model())
        .no_compaction()
        .cwd(temp.path().to_path_buf())
        .session(SessionMode::Dir(temp.path().join("sessions")))
        .thinking_level(ThinkingLevel::Off)
        .with_tool(gate_tool)
        .build()
        .await
        .expect("build a session with the gate tool");

    let drain_task = tokio::spawn({
        let agent = agent.clone();
        async move {
            let mut events = Box::pin(agent.prompt_stream_with_outcome("run both"));
            let mut settled = None;
            let mut outcome = None;
            while let Some(event) = events.as_mut().next().await {
                match event {
                    SdkEvent::AgentOutcome(value) => outcome = Some(value),
                    SdkEvent::AgentSettled { aborted } => {
                        settled = Some(aborted);
                        break;
                    }
                    _ => {}
                }
            }
            (settled, outcome)
        }
    });
    wait_for_tool_entry(&entered).await;
    for _ in 0..400 {
        if entered.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        entered.load(Ordering::SeqCst),
        2,
        "both calls of the same assistant message must be in flight before abort"
    );
    agent.abort();
    let (settled, outcome) = drain_task.await.expect("drain join");
    assert_eq!(
        settled,
        Some(true),
        "the stream must settle with aborted=true"
    );
    let outcome = outcome.expect("aborted run still reports an outcome");
    assert!(outcome.aborted, "the run must be reported as aborted");
    assert_eq!(outcome.tool_results.len(), 2);
    for result in &outcome.tool_results {
        assert!(result.is_error, "aborted call must be an error: {result:?}");
        let rendered = result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(value) => Some(value.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(rendered, "aborted");
    }
    assert!(agent.is_idle(), "an aborted run settles back to idle");

    // The abort must not poison the transcript: the assistant tool-call turn
    // and both error results are persisted, and the next prompt runs cleanly.
    let path = agent.session_path().expect("persistent session file");
    let reopened = SessionManager::open(&path).expect("reopen session file");
    let entries = reopened.build_context_entries().expect("context entries");
    let mut call_ids = Vec::new();
    let mut stored_results = Vec::new();
    for message in &entries {
        match message {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let ContentBlock::ToolCall(call) = block {
                        call_ids.push(call.id.clone());
                    }
                }
            }
            Message::ToolResult(result) => stored_results.push(result),
            _ => {}
        }
    }
    assert_eq!(
        call_ids,
        vec!["call-1".to_owned(), "call-2".to_owned()],
        "both tool calls must survive the abort in session storage"
    );
    assert_eq!(stored_results.len(), 2, "one stored result per call");
    for result in &stored_results {
        assert!(result.is_error, "stored aborted result must be an error");
        assert_eq!(result.tool_name, "gate");
    }

    let recovered = agent.prompt("again").await.expect("prompt after abort");
    assert!(!recovered.aborted);
    assert_eq!(recovered.message_text().as_deref(), Some("recovered"));
    agent.shutdown().await.expect("shutdown");
}

/// A parent SDK run that aborts while the real `subagent` builtin is executing
/// a REAL slow child task (a bash `sleep` that first drops a marker file).
/// Covers the SDK parent/child session boundary on top of the tool-level
/// cancellation test in `tests/subagent_builtin.rs`:
///
/// - the parent stream settles `AgentSettled { aborted: true }` and the
///   `AgentOutcome` is aborted;
/// - the subagent tool result is an error whose details carry the CHILD
///   session state (`stopReason: "aborted"`, agent name) and child usage;
/// - the parent session JSONL persists the assistant `subagent` call plus the
///   aborted tool result;
/// - the session remains usable: the next prompt completes normally.
#[tokio::test]
async fn subagent_abort_mid_real_child_task_reports_aborted_and_recovers() {
    let temp = TempDir::new("subagent-abort");
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("create agents dir");
    std::fs::write(
        agents_dir.join("scout.md"),
        "---\nname: scout\ndescription: Runs slow shell tasks\n---\nBe slow.\n",
    )
    .expect("write scout fixture");
    let marker = temp.path().join("child-started.txt");
    let marker_shell = marker.display().to_string().replace('\\', "/");

    let registry = registry(Arc::new(move |_index, context| {
        let last_user = content_text(last_user_content(context));
        if last_user.contains("delegate the slow task") {
            assistant(
                vec![tool_call_block(
                    "call-1",
                    "subagent",
                    vec![
                        ("agent", serde_json::json!("scout")),
                        ("task", serde_json::json!("run the slow scout task")),
                    ],
                )],
                StopReason::ToolUse,
            )
        } else if last_user.contains("slow scout task") {
            assistant(
                vec![tool_call_block(
                    "call-2",
                    "bash",
                    vec![(
                        "command",
                        serde_json::json!(format!("printf started > '{marker_shell}' && sleep 20")),
                    )],
                )],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text_block("recovered")], StopReason::Stop)
        }
    }));
    let agent = AgentBuilder::new()
        .with_registry(registry, mock_model())
        .agent_dir(temp.path().to_path_buf())
        .no_compaction()
        .cwd(temp.path().to_path_buf())
        .session(SessionMode::Dir(temp.path().join("sessions")))
        .thinking_level(ThinkingLevel::Off)
        .core_tools(true)
        .builtin("subagent")
        .build()
        .await
        .expect("build parent session with the real subagent builtin");

    let drain_task = tokio::spawn({
        let agent = agent.clone();
        async move {
            let mut events = Box::pin(agent.prompt_stream_with_outcome("delegate the slow task"));
            let mut settled = None;
            let mut outcome = None;
            while let Some(event) = events.as_mut().next().await {
                match event {
                    SdkEvent::AgentOutcome(value) => outcome = Some(value),
                    SdkEvent::AgentSettled { aborted } => {
                        settled = Some(aborted);
                        break;
                    }
                    _ => {}
                }
            }
            (settled, outcome)
        }
    });

    // The child's bash command itself proves the real child task started: the
    // marker is written by the child process before it sleeps.
    let mut started = false;
    for _ in 0..500 {
        if marker.exists() {
            started = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        started,
        "the real child bash task never started (marker file missing)"
    );
    agent.abort();
    let (settled, outcome) = drain_task.await.expect("drain join");
    assert_eq!(settled, Some(true), "the parent must settle as aborted");
    let outcome = outcome.expect("aborted run still reports an outcome");
    assert!(outcome.aborted, "the parent outcome must be aborted");
    assert!(agent.is_idle(), "an aborted run settles back to idle");

    // Child session state/usage travel in the subagent tool result details.
    assert_eq!(outcome.tool_results.len(), 1, "one subagent tool result");
    let result = &outcome.tool_results[0];
    assert!(result.is_error, "aborted subagent call must be an error");
    let rendered = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(value) => Some(value.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(rendered, "Subagent was aborted");
    let details = result.details.as_ref().expect("subagent details");
    let child = &details["results"][0];
    assert_eq!(child["agent"], "scout");
    assert_eq!(
        child["stopReason"], "aborted",
        "the child session must report the abort: {child}"
    );
    assert!(
        child["usage"]["turns"].as_u64().unwrap_or(0) >= 1,
        "child usage must be recorded: {child}"
    );
    assert!(
        child["messages"]
            .as_array()
            .is_some_and(|messages| !messages.is_empty()),
        "the child transcript must be kept: {child}"
    );

    // The parent session JSONL persists both the subagent call and the
    // aborted tool result.
    let path = agent.session_path().expect("persistent session file");
    let reopened = SessionManager::open(&path).expect("reopen session file");
    let entries = reopened.build_context_entries().expect("context entries");
    let mut call_names = Vec::new();
    let mut stored_results = Vec::new();
    for message in &entries {
        match message {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let ContentBlock::ToolCall(call) = block {
                        call_names.push(call.name.clone());
                    }
                }
            }
            Message::ToolResult(result) => stored_results.push(result),
            _ => {}
        }
    }
    assert_eq!(
        call_names,
        vec!["subagent".to_owned()],
        "the parent subagent call must survive the abort in session storage"
    );
    assert_eq!(stored_results.len(), 1, "one stored result for the call");
    let stored = stored_results[0];
    assert!(
        stored.is_error,
        "stored aborted subagent result is an error"
    );
    assert_eq!(stored.tool_name, "subagent");
    let stored_text = content_text(&stored.content);
    assert_eq!(stored_text, "Subagent was aborted");

    // The abort must not poison the parent session.
    let recovered = agent.prompt("again").await.expect("prompt after abort");
    assert!(!recovered.aborted);
    assert_eq!(recovered.message_text().as_deref(), Some("recovered"));
    agent.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// SDK tool hooks: real core tools, after-hook rewrite, before-hook block,
// and the stream's tool_execution_start/end projection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sdk_tool_hooks_bracket_real_core_tools_and_project_stream_events() {
    let cwd = TempDir::new("sdk-tool-hooks");
    let read_path = cwd.path().join("note.txt");
    std::fs::write(&read_path, "raw file content\n").expect("write note");
    let edit_path = cwd.path().join("edit.txt");
    std::fs::write(&edit_path, "before\n").expect("write edit target");
    let read_path_arg = read_path.to_string_lossy().to_string();
    let edit_path_arg = edit_path.to_string_lossy().to_string();

    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |index, context| match index {
        0 => assistant(
            vec![tool_call_block(
                "read-1",
                "read",
                vec![("path", json!(read_path_arg))],
            )],
            StopReason::ToolUse,
        ),
        1 => {
            recorded.lock().unwrap().push(context.clone());
            assistant(vec![text_block("read turn done")], StopReason::Stop)
        }
        2 => assistant(
            vec![tool_call_block(
                "edit-1",
                "edit",
                vec![
                    ("path", json!(edit_path_arg)),
                    (
                        "edits",
                        json!([{ "oldText": "before", "newText": "after" }]),
                    ),
                ],
            )],
            StopReason::ToolUse,
        ),
        3 => {
            recorded.lock().unwrap().push(context.clone());
            assistant(vec![text_block("edit turn done")], StopReason::Stop)
        }
        _ => assistant(vec![text_block("done")], StopReason::Stop),
    }));

    let before_log: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let after_log: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let before_for_hook = Arc::clone(&before_log);
    let after_for_hook = Arc::clone(&after_log);

    let agent = agent_builder(registry, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .core_tools(true)
        .no_session()
        .tool_hooks(ToolHooks {
            before: Some(Box::new(move |call, _ctx| {
                let log = Arc::clone(&before_for_hook);
                let name = call.tool_name.clone();
                let args = call.validated_args.clone();
                Box::pin(async move {
                    log.lock().unwrap().push((name.clone(), args));
                    if name == "edit" {
                        BeforeToolCallResult {
                            block: true,
                            reason: Some("hook blocked this edit".to_owned()),
                            terminate: false,
                        }
                    } else {
                        BeforeToolCallResult::default()
                    }
                })
            })),
            after: Some(Box::new(move |call, result, _ctx| {
                let log = Arc::clone(&after_for_hook);
                let name = call.tool_name.clone();
                let raw = result.content.clone();
                let is_error = result.is_error;
                Box::pin(async move {
                    log.lock().unwrap().push((name.clone(), raw, is_error));
                    if name == "read" {
                        AfterToolCallResult {
                            content: Some("HOOK-REWRITTEN READ".to_owned()),
                            details: Some(json!({ "rewritten": true })),
                            ..AfterToolCallResult::default()
                        }
                    } else {
                        AfterToolCallResult::default()
                    }
                })
            })),
        })
        .build()
        .await
        .expect("build hook agent");

    // Prompt 1: the real `read` tool executes; the after hook rewrites the
    // model-facing result and the stream frame carries the REWRITTEN result.
    let mut read_events: Vec<SdkEvent> = Vec::new();
    {
        let mut stream = agent.prompt_stream_with_outcome("read the note");
        while let Some(event) = stream.next().await {
            read_events.push(event);
        }
    }
    let starts: Vec<(String, String)> = read_events
        .iter()
        .filter_map(|event| match event {
            SdkEvent::ToolExecutionStart {
                call_id, tool_name, ..
            } => Some((call_id.clone(), tool_name.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        starts,
        vec![("read-1".to_owned(), "read".to_owned())],
        "exactly one start frame for the one real read call"
    );
    let end_index = read_events
        .iter()
        .position(
            |event| matches!(event, SdkEvent::ToolExecutionEnd { call_id, .. } if call_id == "read-1"),
        )
        .expect("read end frame");
    let start_index = read_events
        .iter()
        .position(
            |event| matches!(event, SdkEvent::ToolExecutionStart { call_id, .. } if call_id == "read-1"),
        )
        .expect("read start frame");
    assert!(
        start_index < end_index,
        "start precedes end: {read_events:?}"
    );
    match &read_events[end_index] {
        SdkEvent::ToolExecutionEnd {
            tool_name,
            is_error,
            result,
            ..
        } => {
            assert_eq!(tool_name, "read");
            assert!(!is_error, "the rewritten read result is successful");
            assert_eq!(result["content"][0]["text"], "HOOK-REWRITTEN READ");
            assert_eq!(result["details"], json!({ "rewritten": true }));
        }
        _ => unreachable!(),
    }
    assert_eq!(
        read_events
            .iter()
            .filter(|event| matches!(event, SdkEvent::ToolExecutionEnd { .. }))
            .count(),
        1,
        "one end frame, no duplicate delivery"
    );

    // The provider's next request carries the same rewritten tool result.
    {
        let contexts = seen.lock().unwrap();
        let context = contexts.first().expect("read follow-up request");
        let result = context
            .messages
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.tool_call_id == "read-1" => Some(result),
                _ => None,
            })
            .expect("read tool result in the follow-up context");
        assert!(!result.is_error);
        assert_eq!(content_text(&result.content), "HOOK-REWRITTEN READ");
        assert_eq!(result.details, Some(json!({ "rewritten": true })));
    }

    // Prompt 2: the before hook blocks the real `edit`; the file is untouched
    // and the stream still has exactly one start + one error end frame.
    let mut edit_events: Vec<SdkEvent> = Vec::new();
    {
        let mut stream = agent.prompt_stream("block the edit");
        while let Some(event) = stream.next().await {
            edit_events.push(event);
        }
    }
    let edit_starts = edit_events
        .iter()
        .filter(
            |event| matches!(event, SdkEvent::ToolExecutionStart { call_id, .. } if call_id == "edit-1"),
        )
        .count();
    assert_eq!(edit_starts, 1, "blocked call still has one start frame");
    let edit_end = edit_events
        .iter()
        .find_map(|event| match event {
            SdkEvent::ToolExecutionEnd {
                call_id,
                is_error,
                result,
                ..
            } if call_id == "edit-1" => Some((*is_error, result.clone())),
            _ => None,
        })
        .expect("blocked edit end frame");
    assert!(edit_end.0, "a blocked call is an error result");
    assert_eq!(edit_end.1["content"][0]["text"], "hook blocked this edit");
    assert_eq!(
        std::fs::read_to_string(&edit_path).expect("read edit target"),
        "before\n",
        "the blocked edit never touched the file"
    );

    // Hooks saw the real validated args; the after hook never ran for the
    // blocked call (it never executed).
    {
        let before = before_log.lock().unwrap();
        assert_eq!(before.len(), 2, "one before hook per call: {before:?}");
        assert_eq!(before[0].0, "read");
        assert_eq!(before[0].1["path"], read_path.to_string_lossy().as_ref());
        assert_eq!(before[1].0, "edit");
        assert_eq!(before[1].1["path"], edit_path.to_string_lossy().as_ref());
        assert_eq!(before[1].1["edits"][0]["oldText"], "before");
    }
    {
        let after = after_log.lock().unwrap();
        assert_eq!(after.len(), 1, "after hook ran only for the executed read");
        assert_eq!(after[0].0, "read");
        assert!(
            after[0].1.contains("raw file content"),
            "after hook saw the raw read result: {:?}",
            after[0]
        );
        assert!(!after[0].2);
    }

    // The blocked result is what the next request carries.
    {
        let contexts = seen.lock().unwrap();
        let context = contexts.last().expect("edit follow-up request");
        let result = context
            .messages
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.tool_call_id == "edit-1" => Some(result),
                _ => None,
            })
            .expect("blocked edit result in the follow-up context");
        assert!(result.is_error);
        assert_eq!(content_text(&result.content), "hook blocked this edit");
    }

    agent.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// SessionMode::Existing: missing file recovers as a new session, corrupt
// non-session fails diagnosably without being modified, malformed interior
// lines are skipped and the transcript still replays.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn existing_session_missing_recovers_corrupt_is_diagnosed_and_bad_lines_skip() {
    let cwd = TempDir::new("sdk-existing-session");
    let ok_registry = registry(Arc::new(|_index, _context| {
        assistant(vec![text_block("ok")], StopReason::Stop)
    }));

    // 1. Missing explicit path: upstream `SessionManager.open` treats it as a
    // new session fixed to that path; the first prompt materializes it.
    let missing = cwd.path().join("sessions").join("missing.jsonl");
    let agent = agent_builder(Arc::clone(&ok_registry), cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Existing(missing.clone()))
        .build()
        .await
        .expect("a missing explicit session path starts a fresh session");
    assert!(agent.session_id().is_some());
    assert_eq!(agent.session_path().as_deref(), Some(missing.as_path()));
    let outcome = agent
        .prompt("first")
        .await
        .expect("prompt on the fresh explicit path");
    assert_eq!(outcome.message_text().as_deref(), Some("ok"));
    assert!(
        missing.exists(),
        "the first prompt creates the explicit file"
    );
    let content = std::fs::read_to_string(&missing).expect("read created session");
    assert!(content.contains("\"type\":\"session\""), "{content}");
    assert!(content.contains("first"), "{content}");
    agent.shutdown().await.expect("shutdown");

    // 2. Corrupt nonempty file: rejected with an actionable Session error and
    // left byte-identical (upstream `_setSessionFile` throws for a non-empty
    // file that does not parse as a session).
    let corrupt = cwd.path().join("corrupt.jsonl");
    let corrupt_bytes = "this is not a session\n{\"broken\": true}\n";
    std::fs::write(&corrupt, corrupt_bytes).expect("write corrupt file");
    let error = agent_builder(Arc::clone(&ok_registry), cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Existing(corrupt.clone()))
        .build()
        .await
        .expect_err("a non-session file must be rejected");
    match &error {
        SdkError::Session(message) => {
            assert!(
                message.contains("not a valid"),
                "diagnostic must say the file is not a session: {message}"
            );
            assert!(
                message.contains("corrupt.jsonl"),
                "diagnostic must name the file: {message}"
            );
        }
        other => panic!("expected SdkError::Session, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&corrupt).expect("read corrupt file"),
        corrupt_bytes,
        "the rejected file is never modified"
    );

    // 3. Valid session + malformed interior line: malformed lines are skipped
    // (upstream `loadEntriesFromFile`), the prior transcript still replays and
    // a follow-up prompt works.
    let mut with_bad_line = std::fs::read_to_string(&missing).expect("read valid session");
    with_bad_line.push_str("this line is not json\n");
    std::fs::write(&missing, &with_bad_line).expect("append malformed line");

    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let replay_registry = registry(Arc::new(move |_index, context| {
        recorded.lock().unwrap().push(context.clone());
        assistant(vec![text_block("second ok")], StopReason::Stop)
    }));
    let resumed = agent_builder(replay_registry, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Existing(missing.clone()))
        .build()
        .await
        .expect("malformed interior lines are skipped, not fatal");
    let outcome = resumed
        .prompt("second")
        .await
        .expect("prompt on the recovered session");
    assert_eq!(outcome.message_text().as_deref(), Some("second ok"));
    {
        let contexts = seen.lock().unwrap();
        let context = contexts.last().expect("replayed request");
        let user_texts: Vec<String> = context
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => Some(
                    user.content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(
            user_texts.iter().any(|text| text == "first"),
            "the prior user turn replayed: {user_texts:?}"
        );
        assert!(user_texts.iter().any(|text| text == "second"));
    }
    resumed.shutdown().await.expect("shutdown");

    // 4. attach_session to a path whose file was deleted: treated as a fresh
    // session at that path, usable immediately.
    let deleted_path = cwd.path().join("sessions").join("deleted.jsonl");
    let dir_agent = agent_builder(Arc::clone(&ok_registry), cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Dir(cwd.path().join("live-sessions")))
        .build()
        .await
        .expect("build dir agent");
    dir_agent
        .attach_session(&deleted_path)
        .await
        .expect("attaching a deleted/missing path starts a new session there");
    assert!(dir_agent.session_id().is_some());
    assert_eq!(
        dir_agent.session_path().as_deref(),
        Some(deleted_path.as_path())
    );
    let outcome = dir_agent
        .prompt("third")
        .await
        .expect("prompt after attach to a missing path");
    assert_eq!(outcome.message_text().as_deref(), Some("ok"));
    assert!(
        deleted_path.exists(),
        "the prompt recreates the attached path"
    );
    dir_agent.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// `background: true`: the before hook runs at preflight, the after hook runs
// inside the detached execution, and the rewritten result is what the
// BackgroundToolEnd event and the persisted `backgroundToolResult` entry
// carry. The requesting turn returns on the placeholder, never waiting for
// the detached command.
// ---------------------------------------------------------------------------

type BackgroundEndRecord = (
    String,
    bool,
    String,
    Option<serde_json::Value>,
    Option<Usage>,
);

async fn wait_for_background_end(
    events: &Arc<Mutex<Vec<BackgroundEndRecord>>>,
    call_id: &str,
) -> BackgroundEndRecord {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(record) = events
            .lock()
            .unwrap()
            .iter()
            .find(|record| record.0 == call_id)
            .cloned()
        {
            return record;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no BackgroundToolEnd for {call_id} within 20s: {:?}",
            events.lock().unwrap()
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn background_tool_hooks_rewrite_completion_and_never_block_the_turn() {
    let cwd = TempDir::new("sdk-background-hooks");
    let marker = cwd.path().join("bg-marker.txt");
    // The bash tool runs the command in a POSIX shell: use forward slashes so
    // a Windows temp path is not read as a sequence of backslash escapes.
    let marker_arg = marker.to_string_lossy().replace('\\', "/");
    let sessions = cwd.path().join("sessions");

    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |index, context| match index {
        0 => assistant(
            vec![tool_call_block(
                "bg-ok",
                "bash",
                vec![
                    (
                        "command",
                        json!(format!("sleep 3; printf done > {marker_arg}")),
                    ),
                    ("background", json!(true)),
                ],
            )],
            StopReason::ToolUse,
        ),
        1 => {
            recorded.lock().unwrap().push(context.clone());
            assistant(vec![text_block("background started")], StopReason::Stop)
        }
        2 => assistant(
            vec![tool_call_block(
                "bg-fail",
                "bash",
                vec![
                    ("command", json!("printf boom >&2; exit 7")),
                    ("background", json!(true)),
                ],
            )],
            StopReason::ToolUse,
        ),
        3 => assistant(
            vec![text_block("failing background started")],
            StopReason::Stop,
        ),
        4 => assistant(
            vec![tool_call_block(
                "bg-abort",
                "bash",
                vec![("command", json!("sleep 30")), ("background", json!(true))],
            )],
            StopReason::ToolUse,
        ),
        _ => assistant(vec![text_block("abort started")], StopReason::Stop),
    }));

    let before_log: Arc<Mutex<Vec<(String, String, serde_json::Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let after_log: Arc<Mutex<Vec<(String, String, String, bool)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let before_for_hook = Arc::clone(&before_log);
    let after_for_hook = Arc::clone(&after_log);

    let agent = agent_builder(registry, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Dir(sessions.clone()))
        .core_tools(true)
        .tool_hooks(ToolHooks {
            before: Some(Box::new(move |call, _ctx| {
                let log = Arc::clone(&before_for_hook);
                let call_id = call.call_id.clone();
                let name = call.tool_name.clone();
                let args = call.validated_args.clone();
                Box::pin(async move {
                    log.lock().unwrap().push((call_id, name, args));
                    BeforeToolCallResult::default()
                })
            })),
            after: Some(Box::new(move |call, result, _ctx| {
                let log = Arc::clone(&after_for_hook);
                let call_id = call.call_id.clone();
                let name = call.tool_name.clone();
                let raw = result.content.clone();
                let is_error = result.is_error;
                Box::pin(async move {
                    log.lock().unwrap().push((call_id, name, raw, is_error));
                    if is_error {
                        AfterToolCallResult {
                            content: Some("HOOK-REWRITTEN BACKGROUND ERROR".to_owned()),
                            details: Some(json!({ "rewritten_error": true })),
                            // The after hook's usage must reach the
                            // completion's event and durable entry: a
                            // background call has no tool-result message.
                            usage: Some(Usage {
                                input: 5,
                                output: 2,
                                total_tokens: 7,
                                ..Usage::default()
                            }),
                            ..AfterToolCallResult::default()
                        }
                    } else {
                        AfterToolCallResult {
                            content: Some("HOOK-REWRITTEN BACKGROUND".to_owned()),
                            details: Some(json!({ "background_rewritten": true })),
                            usage: Some(Usage {
                                input: 7,
                                output: 3,
                                total_tokens: 10,
                                ..Usage::default()
                            }),
                            ..AfterToolCallResult::default()
                        }
                    }
                })
            })),
        })
        .build()
        .await
        .expect("build background-hook agent");

    let bg_events: Arc<Mutex<Vec<BackgroundEndRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&bg_events);
    agent.on_session_event(move |event| {
        if let Some(SdkEvent::BackgroundToolEnd {
            call_id,
            is_error,
            output,
            details,
            usage,
            ..
        }) = SdkEvent::from_session_event(&event)
        {
            captured
                .lock()
                .unwrap()
                .push((call_id, is_error, output, details, usage));
        }
    });

    // Prompt 1: a real 3s bash command detaches; the turn must settle on the
    // placeholder immediately (well before the command finishes).
    let started = std::time::Instant::now();
    let outcome = agent
        .prompt("start the background job")
        .await
        .expect("background prompt");
    let elapsed = started.elapsed();
    assert_eq!(
        outcome.message_text().as_deref(),
        Some("background started")
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "the main loop must not wait for the detached 3s command (took {elapsed:?})"
    );
    {
        let contexts = seen.lock().unwrap();
        let context = contexts.first().expect("placeholder follow-up request");
        let result = context
            .messages
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.tool_call_id == "bg-ok" => Some(result),
                _ => None,
            })
            .expect("placeholder tool result in the follow-up context");
        assert!(!result.is_error);
        let text = content_text(&result.content);
        assert!(
            text.contains("running in the background"),
            "the placeholder announces the detached call: {text}"
        );
    }

    let (call_id, is_error, output, details, usage) =
        wait_for_background_end(&bg_events, "bg-ok").await;
    assert_eq!(call_id, "bg-ok");
    assert!(!is_error);
    assert_eq!(
        output, "HOOK-REWRITTEN BACKGROUND",
        "the after-hook rewrite is the completion's output"
    );
    assert_eq!(details, Some(json!({ "background_rewritten": true })));
    assert_eq!(
        usage,
        Some(Usage {
            input: 7,
            output: 3,
            total_tokens: 10,
            ..Usage::default()
        }),
        "the after-hook usage reaches the completion event"
    );
    assert!(marker.exists(), "the detached command really ran");

    // The durable `backgroundToolResult` entry carries the rewritten result.
    let session_path = agent.session_path().expect("persistent background session");
    let persisted = std::fs::read_to_string(&session_path).expect("read session file");
    assert!(
        persisted.contains("backgroundToolResult"),
        "completion persisted as a backgroundToolResult entry: {persisted}"
    );
    assert!(
        persisted.contains("HOOK-REWRITTEN BACKGROUND"),
        "the persisted entry carries the rewritten output: {persisted}"
    );
    assert!(
        persisted.contains("\"usage\"")
            && persisted.contains("\"input\":7")
            && persisted.contains("\"output\":3")
            && persisted.contains("\"totalTokens\":10"),
        "the persisted backgroundToolResult entry carries the after-hook usage: {persisted}"
    );

    // Exactly one completion event for the call.
    assert_eq!(
        bg_events
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.0 == "bg-ok")
            .count(),
        1,
        "one BackgroundToolEnd per detached completion"
    );

    // Prompt 2: a failing detached command. The after hook sees the real
    // error result and its rewrite stays an error completion.
    let fail_outcome = agent
        .prompt("start the failing background job")
        .await
        .expect("failing background prompt");
    assert_eq!(
        fail_outcome.message_text().as_deref(),
        Some("failing background started")
    );
    let (call_id, is_error, output, details, usage) =
        wait_for_background_end(&bg_events, "bg-fail").await;
    assert_eq!(call_id, "bg-fail");
    assert!(is_error, "a failing detached command completes as an error");
    assert_eq!(output, "HOOK-REWRITTEN BACKGROUND ERROR");
    assert_eq!(details, Some(json!({ "rewritten_error": true })));
    assert_eq!(
        usage,
        Some(Usage {
            input: 5,
            output: 2,
            total_tokens: 7,
            ..Usage::default()
        }),
        "a failing completion still carries the after-hook usage"
    );

    {
        let before = before_log.lock().unwrap();
        assert_eq!(
            before.len(),
            2,
            "one before hook per background call: {before:?}"
        );
        assert_eq!(before[0].0, "bg-ok");
        assert_eq!(before[0].1, "bash");
        assert!(
            before[0].2["command"]
                .as_str()
                .is_some_and(|value| value.contains("sleep 3")),
            "validated args reach the before hook: {:?}",
            before[0].2
        );
        assert!(
            before[0].2.get("background").is_none(),
            "the scheduler strips the background flag before validation: {:?}",
            before[0].2
        );
        assert_eq!(before[1].0, "bg-fail");
    }
    {
        let after = after_log.lock().unwrap();
        let ok = after
            .iter()
            .find(|entry| entry.0 == "bg-ok")
            .expect("after hook ran for the successful detached call");
        assert_eq!(ok.1, "bash");
        assert!(!ok.3);
        let failed = after
            .iter()
            .find(|entry| entry.0 == "bg-fail")
            .expect("after hook ran for the failing detached call");
        assert!(
            failed.3,
            "the after hook saw the real error result: {failed:?}"
        );
        assert!(
            !failed.2.is_empty(),
            "the after hook sees the real error text: {failed:?}"
        );
    }

    // Prompt 3: abort an in-flight background call. `shutdown` cancels every
    // detached call; an aborted completion is never delivered (§2.5) and the
    // after hook never runs for it.
    let abort_outcome = agent
        .prompt("start the abortable background job")
        .await
        .expect("abortable background prompt");
    assert_eq!(
        abort_outcome.message_text().as_deref(),
        Some("abort started")
    );
    agent
        .shutdown()
        .await
        .expect("shutdown aborts background calls");
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        !bg_events
            .lock()
            .unwrap()
            .iter()
            .any(|record| record.0 == "bg-abort"),
        "an aborted background call must not deliver a completion"
    );
    assert!(
        !after_log
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry.0 == "bg-abort"),
        "the after hook never runs for an aborted detached call"
    );
    assert!(
        before_log
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry.0 == "bg-abort"),
        "the before hook ran at preflight for the aborted background call"
    );
}

// ---------------------------------------------------------------------------
// `background: true` + a before-hook block: the block happens at preflight,
// so the detached command never starts, no BackgroundToolEnd is emitted, the
// main loop settles immediately, and the after hook never runs. A blocked
// call with `terminate: true` ends the tool phase; a blocked call with
// `terminate: false` lets the turn continue to the next model request.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn background_tool_blocked_by_before_hook_never_starts_and_never_completes() {
    let cwd = TempDir::new("sdk-background-block");
    let terminate_marker = cwd.path().join("blocked-terminate.txt");
    let continue_marker = cwd.path().join("blocked-continue.txt");
    let terminate_arg = terminate_marker.to_string_lossy().replace('\\', "/");
    let continue_arg = continue_marker.to_string_lossy().replace('\\', "/");
    let sessions = cwd.path().join("sessions");

    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |index, context| match index {
        0 => assistant(
            vec![tool_call_block(
                "bg-blocked-terminate",
                "bash",
                vec![
                    ("command", json!(format!("printf ran > {terminate_arg}"))),
                    ("background", json!(true)),
                ],
            )],
            StopReason::ToolUse,
        ),
        1 => assistant(
            vec![tool_call_block(
                "bg-blocked-continue",
                "bash",
                vec![
                    ("command", json!(format!("printf ran > {continue_arg}"))),
                    ("background", json!(true)),
                ],
            )],
            StopReason::ToolUse,
        ),
        2 => {
            recorded.lock().unwrap().push(context.clone());
            assistant(vec![text_block("continued after block")], StopReason::Stop)
        }
        _ => assistant(vec![text_block("unexpected")], StopReason::Stop),
    }));

    let before_log: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let after_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let before_for_hook = Arc::clone(&before_log);
    let after_for_hook = Arc::clone(&after_log);

    let agent = agent_builder(registry, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Dir(sessions.clone()))
        .core_tools(true)
        .tool_hooks(ToolHooks {
            before: Some(Box::new(move |call, _ctx| {
                let log = Arc::clone(&before_for_hook);
                let call_id = call.call_id.clone();
                let args = call.validated_args.clone();
                Box::pin(async move {
                    log.lock().unwrap().push((call_id.clone(), args));
                    if call_id == "bg-blocked-terminate" {
                        BeforeToolCallResult {
                            block: true,
                            reason: Some("policy: no background writes".to_owned()),
                            terminate: true,
                        }
                    } else {
                        BeforeToolCallResult {
                            block: true,
                            reason: Some("policy: no background writes here".to_owned()),
                            terminate: false,
                        }
                    }
                })
            })),
            after: Some(Box::new(move |call, _result, _ctx| {
                let log = Arc::clone(&after_for_hook);
                let call_id = call.call_id.clone();
                Box::pin(async move {
                    log.lock().unwrap().push(call_id);
                    AfterToolCallResult::default()
                })
            })),
        })
        .build()
        .await
        .expect("build background-block agent");

    let bg_events: Arc<Mutex<Vec<BackgroundEndRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&bg_events);
    agent.on_session_event(move |event| {
        if let Some(SdkEvent::BackgroundToolEnd { call_id, .. }) =
            SdkEvent::from_session_event(&event)
        {
            captured
                .lock()
                .unwrap()
                .push((call_id, false, String::new(), None, None));
        }
    });

    // Prompt 1: blocked with terminate=true. The turn ends without another
    // model request and without waiting for any detached execution.
    let started = std::time::Instant::now();
    let outcome = agent
        .prompt("try a blocked background job")
        .await
        .expect("blocked background prompt");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "a blocked background call must not block the turn (took {elapsed:?})"
    );
    assert!(
        outcome
            .tool_result_texts()
            .join("\n")
            .contains("policy: no background writes"),
        "the blocked result carries the hook reason: {:?}",
        outcome.tool_result_texts()
    );

    // Prompt 2: blocked with terminate=false; the turn continues to the next
    // model request.
    let continued = agent
        .prompt("try another blocked background job")
        .await
        .expect("second blocked background prompt");
    assert_eq!(
        continued.message_text().as_deref(),
        Some("continued after block"),
        "a non-terminating block lets the turn continue"
    );
    assert!(
        continued
            .tool_result_texts()
            .join("\n")
            .contains("policy: no background writes here"),
        "the second blocked result carries its reason: {:?}",
        continued.tool_result_texts()
    );
    {
        let contexts = seen.lock().unwrap();
        let context = contexts.last().expect("follow-up request after the block");
        let result = context
            .messages
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.tool_call_id == "bg-blocked-continue" => {
                    Some(result)
                }
                _ => None,
            })
            .expect("blocked background result in the follow-up context");
        assert!(result.is_error);
        assert_eq!(
            content_text(&result.content),
            "policy: no background writes here"
        );
    }

    // The detached command never started: no marker, no completion event, no
    // after-hook invocation.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        !terminate_marker.exists(),
        "the blocked command must not run (terminate case)"
    );
    assert!(
        !continue_marker.exists(),
        "the blocked command must not run (continue case)"
    );
    assert!(
        bg_events.lock().unwrap().is_empty(),
        "a blocked background call never delivers a completion: {:?}",
        bg_events.lock().unwrap()
    );
    assert!(
        after_log.lock().unwrap().is_empty(),
        "the after hook never runs for a blocked call: {:?}",
        after_log.lock().unwrap()
    );
    {
        let before = before_log.lock().unwrap();
        assert_eq!(
            before.len(),
            2,
            "one before hook per blocked call: {before:?}"
        );
        assert_eq!(before[0].0, "bg-blocked-terminate");
        assert_eq!(before[1].0, "bg-blocked-continue");
        for (_, args) in before.iter() {
            assert!(
                args.get("background").is_none(),
                "the scheduler strips the background flag before the hook: {args:?}"
            );
        }
    }
    agent.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Two agents: distinct agent_dir/session dir; concurrent prompts must keep
// todo state, session transcripts, events, and usage isolated.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_agents_isolate_todo_sessions_events_and_usage_under_concurrent_prompts() {
    let cwd = TempDir::new("sdk-dual-agent");
    let sessions_a = cwd.path().join("sessions-a");
    let sessions_b = cwd.path().join("sessions-b");

    let make_registry =
        |call_prefix: &'static str, subject: &'static str, input: u64, output: u64| {
            registry(Arc::new(move |index, _context| match index {
                0 => assistant(
                    vec![tool_call_block(
                        &format!("{call_prefix}-create"),
                        "todo",
                        vec![("action", json!("create")), ("subject", json!(subject))],
                    )],
                    StopReason::ToolUse,
                ),
                1 => assistant(
                    vec![tool_call_block(
                        &format!("{call_prefix}-list"),
                        "todo",
                        vec![("action", json!("list"))],
                    )],
                    StopReason::ToolUse,
                ),
                _ => {
                    let mut message = assistant(vec![text_block("todo done")], StopReason::Stop);
                    message.usage = Usage {
                        input,
                        output,
                        total_tokens: input + output,
                        ..Usage::default()
                    };
                    message
                }
            }))
        };

    let agent_a = agent_builder(make_registry("a", "alpha task", 11, 3), cwd.path())
        .agent_dir(cwd.path().join("agent-a"))
        .session(SessionMode::Dir(sessions_a.clone()))
        .builtin("todo")
        .build()
        .await
        .expect("build agent a");

    let agent_b = agent_builder(make_registry("b", "beta task", 77, 5), cwd.path())
        .agent_dir(cwd.path().join("agent-b"))
        .session(SessionMode::Dir(sessions_b.clone()))
        .builtin("todo")
        .build()
        .await
        .expect("build agent b");

    let entries_a: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let entries_b: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let entries = Arc::clone(&entries_a);
        agent_a.on_session_event(move |event| {
            if let pir::sdk::SessionEvent::EntryAppended { entry } = &event {
                entries.lock().unwrap().push(entry.to_string());
            }
        });
    }
    {
        let entries = Arc::clone(&entries_b);
        agent_b.on_session_event(move |event| {
            if let pir::sdk::SessionEvent::EntryAppended { entry } = &event {
                entries.lock().unwrap().push(entry.to_string());
            }
        });
    }

    let (outcome_a, outcome_b) = tokio::join!(
        agent_a.prompt("create alpha"),
        agent_b.prompt("create beta")
    );
    let outcome_a = outcome_a.expect("agent a prompt");
    let outcome_b = outcome_b.expect("agent b prompt");

    // Todo state: each agent's own tool results carry only its own task.
    let results_a = outcome_a.tool_result_texts().join("\n");
    let results_b = outcome_b.tool_result_texts().join("\n");
    assert!(
        results_a.contains("alpha task") && !results_a.contains("beta task"),
        "agent a sees only its todo list: {results_a}"
    );
    assert!(
        results_b.contains("beta task") && !results_b.contains("alpha task"),
        "agent b sees only its todo list: {results_b}"
    );

    // Session files: distinct paths and contents; no cross-writing.
    let path_a = agent_a.session_path().expect("session a");
    let path_b = agent_b.session_path().expect("session b");
    assert_ne!(path_a, path_b);
    assert!(
        path_a.starts_with(&sessions_a),
        "session a under its dir: {path_a:?}"
    );
    assert!(
        path_b.starts_with(&sessions_b),
        "session b under its dir: {path_b:?}"
    );
    let file_a = std::fs::read_to_string(&path_a).expect("read session a");
    let file_b = std::fs::read_to_string(&path_b).expect("read session b");
    assert!(
        file_a.contains("alpha task") && !file_a.contains("beta task"),
        "{file_a}"
    );
    assert!(
        file_b.contains("beta task") && !file_b.contains("alpha task"),
        "{file_b}"
    );

    // Session-level events: each subscription saw only its own transcript.
    let events_a = entries_a.lock().unwrap().clone();
    let events_b = entries_b.lock().unwrap().clone();
    assert!(!events_a.is_empty() && !events_b.is_empty());
    let joined_a = events_a.join("\n");
    let joined_b = events_b.join("\n");
    assert!(
        joined_a.contains("alpha task") && !joined_a.contains("beta task"),
        "agent a's events leaked agent b's transcript"
    );
    assert!(
        joined_b.contains("beta task") && !joined_b.contains("alpha task"),
        "agent b's events leaked agent a's transcript"
    );

    // Usage: the terminal-message usage is per agent (11+3 vs 77+5).
    assert_eq!(outcome_a.usage.total_tokens, 14, "{:?}", outcome_a.usage);
    assert_eq!(outcome_b.usage.total_tokens, 82, "{:?}", outcome_b.usage);
    assert_eq!(agent_a.usage_totals().total_tokens, 14);
    assert_eq!(agent_b.usage_totals().total_tokens, 82);
    // Three provider requests per run (create, list, final text).
    assert_eq!(agent_a.usage_totals().turns, 3);
    assert_eq!(agent_b.usage_totals().turns, 3);

    agent_a.shutdown().await.expect("shutdown a");
    agent_b.shutdown().await.expect("shutdown b");
}

// ---------------------------------------------------------------------------
// Two agents writing the SAME path concurrently: writes are atomic
// replacements, so the final file must be exactly one complete writer's
// payload — never a torn/interleaved mix.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_agents_writing_the_same_path_concurrently_never_tear_the_file() {
    let cwd = TempDir::new("sdk-dual-write");
    let target = cwd.path().join("shared.txt");
    let target_arg = target.to_string_lossy().to_string();
    let content_a = "A".repeat(32768);
    let content_b = "B".repeat(32768);

    // Both writers target the same path while it is still a new file. Which
    // write tool reaches its read-before-write gate first is scheduler
    // dependent: if one write lands before the other's gate stats the target,
    // the late writer sees an existing file it never read and is refused with
    // the documented gate error (new-file writes bypass the check only while
    // the target does not exist). That refusal is correct product behaviour;
    // this test therefore pins the file-integrity invariant under either
    // interleaving instead of depending on both writes being admitted:
    // at least one writer succeeds and the final file is exactly one
    // complete 32 KiB payload - never torn, truncated, or mixed.

    let make_registry = |content: String| {
        let target_arg = target_arg.clone();
        registry(Arc::new(move |index, _context| match index {
            0 => assistant(
                vec![tool_call_block(
                    "w-1",
                    "write",
                    vec![("path", json!(target_arg)), ("content", json!(content))],
                )],
                StopReason::ToolUse,
            ),
            _ => assistant(vec![text_block("written")], StopReason::Stop),
        }))
    };

    let agent_a = agent_builder(make_registry(content_a.clone()), cwd.path())
        .agent_dir(cwd.path().join("agent-a"))
        .session(SessionMode::Dir(cwd.path().join("sessions-a")))
        .core_tools(true)
        .build()
        .await
        .expect("build writer a");
    let agent_b = agent_builder(make_registry(content_b.clone()), cwd.path())
        .agent_dir(cwd.path().join("agent-b"))
        .session(SessionMode::Dir(cwd.path().join("sessions-b")))
        .core_tools(true)
        .build()
        .await
        .expect("build writer b");

    let (outcome_a, outcome_b) = tokio::join!(agent_a.prompt("write A"), agent_b.prompt("write B"));
    let outcome_a = outcome_a.expect("writer a prompt");
    let outcome_b = outcome_b.expect("writer b prompt");
    assert_eq!(outcome_a.tool_results.len(), 1, "writer a issues one write");
    assert_eq!(outcome_b.tool_results.len(), 1, "writer b issues one write");

    // A writer either completed its write or was refused by the
    // read-before-write gate with one of its documented refusal texts. Any
    // other failure (I/O error, panic surface, ...) is a real defect.
    let is_gate_refusal = |text: &str| {
        text.contains("has not been read in this session")
            || text.contains("changed since it was last read")
            || text.contains("only part of the file has been read in this session")
    };
    let succeeded_a = !outcome_a.tool_results[0].is_error;
    let succeeded_b = !outcome_b.tool_results[0].is_error;
    let refusal_text_a = outcome_a.tool_result_texts().join("\n");
    let refusal_text_b = outcome_b.tool_result_texts().join("\n");
    if !succeeded_a {
        assert!(
            is_gate_refusal(&refusal_text_a),
            "writer a failed for a non-gate reason: {:?}",
            outcome_a.tool_result_texts()
        );
    }
    if !succeeded_b {
        assert!(
            is_gate_refusal(&refusal_text_b),
            "writer b failed for a non-gate reason: {:?}",
            outcome_b.tool_result_texts()
        );
    }
    assert!(
        succeeded_a || succeeded_b,
        "at least one concurrent writer must succeed; a: {:?}, b: {:?}",
        outcome_a.tool_result_texts(),
        outcome_b.tool_result_texts()
    );

    let final_content = std::fs::read_to_string(&target).expect("read shared target");
    assert!(
        final_content == content_a || final_content == content_b,
        "the concurrent writes must leave one COMPLETE payload (len {}, expected 32768)",
        final_content.len()
    );
    // Tie the final bytes to the writer that actually succeeded: a refused
    // writer must not have left any partial or mixed content behind.
    if !succeeded_a {
        assert_eq!(
            final_content, content_b,
            "writer a was refused, so writer b's payload must remain whole"
        );
    }
    if !succeeded_b {
        assert_eq!(
            final_content, content_a,
            "writer b was refused, so writer a's payload must remain whole"
        );
    }

    agent_a.shutdown().await.expect("shutdown a");
    agent_b.shutdown().await.expect("shutdown b");
}

// ---------------------------------------------------------------------------
// background usage precedence: the `afterToolCall` hook's report is the base
// and a `tool_result` handler's PARSEABLE report replaces it (upstream
// agent-loop.ts `usage: afterResult.usage ?? result.usage`). A handler that
// merges other fields without reporting usage must not wipe the hook value.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn background_tool_result_handler_usage_overrides_hook_and_absent_handler_usage_preserves_it()
{
    let cwd = TempDir::new("sdk-background-usage-precedence");
    let sessions = cwd.path().join("sessions");

    let registry = registry(Arc::new(|index, _context| match index {
        0 => assistant(
            vec![tool_call_block(
                "bg-hook-only",
                "bash",
                vec![
                    ("command", json!("printf hook-only")),
                    ("background", json!(true)),
                ],
            )],
            StopReason::ToolUse,
        ),
        1 => assistant(vec![text_block("started hook-only")], StopReason::Stop),
        2 => assistant(
            vec![tool_call_block(
                "bg-handler-override",
                "bash",
                vec![
                    ("command", json!("printf override")),
                    ("background", json!(true)),
                ],
            )],
            StopReason::ToolUse,
        ),
        3 => assistant(vec![text_block("started override")], StopReason::Stop),
        4 => assistant(
            vec![tool_call_block(
                "bg-details-only",
                "bash",
                vec![
                    ("command", json!("printf details")),
                    ("background", json!(true)),
                ],
            )],
            StopReason::ToolUse,
        ),
        5 => assistant(vec![text_block("started details")], StopReason::Stop),
        _ => assistant(vec![text_block("unexpected")], StopReason::Stop),
    }));

    let agent = agent_builder(registry, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Dir(sessions.clone()))
        .core_tools(true)
        .on_event("tool_result", |payload| {
            let call_id = payload["toolCallId"].as_str().unwrap_or_default();
            match call_id {
                "bg-handler-override" => PluginEventOutcome::ToolResult {
                    content: Some(json!("HANDLER-REWRITTEN BACKGROUND")),
                    details: Some(json!({ "handler_override": true })),
                    structured_content: None,
                    is_error: None,
                    usage: Some(json!({ "input": 3, "output": 4, "totalTokens": 7 })),
                },
                "bg-details-only" => PluginEventOutcome::ToolResult {
                    content: None,
                    details: Some(json!({ "handler_note": "saw the completion" })),
                    structured_content: None,
                    is_error: None,
                    usage: None,
                },
                // bg-hook-only: the handler has no opinion at all.
                _ => PluginEventOutcome::None,
            }
        })
        .tool_hooks(ToolHooks {
            before: None,
            after: Some(Box::new(|_call, _result, _ctx| {
                Box::pin(async move {
                    AfterToolCallResult {
                        content: Some("HOOK-REWRITTEN BACKGROUND".to_owned()),
                        details: Some(json!({ "hook_rewritten": true })),
                        usage: Some(Usage {
                            input: 7,
                            output: 3,
                            total_tokens: 10,
                            ..Usage::default()
                        }),
                        ..AfterToolCallResult::default()
                    }
                })
            })),
        })
        .build()
        .await
        .expect("build background usage-precedence agent");

    let bg_events: Arc<Mutex<Vec<BackgroundEndRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&bg_events);
    agent.on_session_event(move |event| {
        if let Some(SdkEvent::BackgroundToolEnd {
            call_id,
            is_error,
            output,
            details,
            usage,
            ..
        }) = SdkEvent::from_session_event(&event)
        {
            captured
                .lock()
                .unwrap()
                .push((call_id, is_error, output, details, usage));
        }
    });

    let hook_usage = Usage {
        input: 7,
        output: 3,
        total_tokens: 10,
        ..Usage::default()
    };
    let handler_usage = Usage {
        input: 3,
        output: 4,
        total_tokens: 7,
        ..Usage::default()
    };

    // 1) Handler returns no opinion: the hook's usage survives.
    agent
        .prompt("run hook-only")
        .await
        .expect("hook-only prompt");
    let (_, is_error, output, _, usage) = wait_for_background_end(&bg_events, "bg-hook-only").await;
    assert!(!is_error);
    assert_eq!(output, "HOOK-REWRITTEN BACKGROUND");
    assert_eq!(
        usage,
        Some(hook_usage.clone()),
        "a handler that reports nothing must not erase the hook usage"
    );

    // 2) Handler reports usage: it overrides the hook's report (upstream ??).
    agent
        .prompt("run handler-override")
        .await
        .expect("handler-override prompt");
    let (_, is_error, output, details, usage) =
        wait_for_background_end(&bg_events, "bg-handler-override").await;
    assert!(!is_error);
    assert_eq!(output, "HANDLER-REWRITTEN BACKGROUND");
    assert_eq!(details, Some(json!({ "handler_override": true })));
    assert_eq!(
        usage,
        Some(handler_usage.clone()),
        "a parseable handler usage replaces the hook usage"
    );

    // 3) Handler merges details only: the hook's usage is still the value.
    agent
        .prompt("run details-only")
        .await
        .expect("details-only prompt");
    let (_, is_error, output, details, usage) =
        wait_for_background_end(&bg_events, "bg-details-only").await;
    assert!(!is_error);
    assert_eq!(output, "HOOK-REWRITTEN BACKGROUND");
    assert_eq!(
        details,
        Some(json!({ "handler_note": "saw the completion" }))
    );
    assert_eq!(
        usage,
        Some(hook_usage),
        "an unrelated merge (details only) must not clear the hook usage"
    );

    // The durable entries carry the same precedence, per call.
    let session_path = agent.session_path().expect("persistent background session");
    let persisted = std::fs::read_to_string(&session_path).expect("read session file");
    let mut entries = std::collections::HashMap::new();
    for line in persisted.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("customType").and_then(|tag| tag.as_str()) != Some("backgroundToolResult") {
            continue;
        }
        let call_id = value["details"]["callId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        entries.insert(call_id, value);
    }
    assert_eq!(
        entries.len(),
        3,
        "one durable entry per background completion: {persisted}"
    );
    for (call_id, expected) in [
        ("bg-hook-only", &hook_usage),
        ("bg-handler-override", &handler_usage),
    ] {
        let entry = entries
            .get(call_id)
            .unwrap_or_else(|| panic!("missing persisted entry for {call_id}: {persisted}"));
        assert_eq!(
            entry["details"]["usage"]["input"].as_u64(),
            Some(expected.input),
            "{call_id} persisted input usage: {entry}"
        );
        assert_eq!(
            entry["details"]["usage"]["output"].as_u64(),
            Some(expected.output),
            "{call_id} persisted output usage: {entry}"
        );
        assert_eq!(
            entry["details"]["usage"]["totalTokens"].as_u64(),
            Some(expected.total_tokens),
            "{call_id} persisted total usage: {entry}"
        );
    }
    let details_only = entries
        .get("bg-details-only")
        .unwrap_or_else(|| panic!("missing persisted entry for bg-details-only: {persisted}"));
    assert_eq!(
        details_only["details"]["usage"]["input"].as_u64(),
        Some(7),
        "details-only entry keeps the hook usage: {details_only}"
    );
    assert_eq!(
        details_only["details"]["details"]["handler_note"].as_str(),
        Some("saw the completion"),
        "the handler merge is persisted: {details_only}"
    );

    agent.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// prompt_stream backpressure contract (docs/sdk.md "Events"): intermediate
// delivery is best-effort (`try_send`, a full channel drops), while the
// terminal AgentSettled is sent with backpressure after settlement. A slow
// consumer must never block the loop or panic, and surviving events must
// keep provider order with the terminal event last.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prompt_stream_slow_consumer_drops_intermediate_but_delivers_terminal_in_order() {
    const DELTAS: usize = 300;
    let cwd = TempDir::new("sdk-stream-slow-consumer");

    let registry = frame_registry(Arc::new(|_index| {
        let full: String = (0..DELTAS).map(|i| format!("d{i:03};")).collect();
        let message = assistant(vec![text_block(&full)], StopReason::Stop);
        let mut frames = Vec::with_capacity(DELTAS + 1);
        for i in 0..DELTAS {
            frames.push(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: format!("d{i:03};"),
                partial: Arc::new(message.clone()),
            });
        }
        frames.push(AssistantMessageEvent::Done {
            reason: TerminalReason::Stop,
            message,
        });
        frames
    }));
    let agent = agent_builder(registry, cwd.path())
        .no_session()
        .build()
        .await
        .expect("build flood agent");
    // Small bounded channel: 300 deltas cannot fit; the terminal send must
    // park on the full channel until the consumer resumes. The capacity is
    // still large enough that the trailing burst window contains deltas, so
    // their relative order can be asserted too.
    agent.set_event_channel_capacity(32);

    let mut stream = agent.prompt_stream("flood the stream");
    // Real slow consumer: do not poll for a while, so the burst overflows the
    // channel and the run settles with the terminal event still parked.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }

    let settled_count = events
        .iter()
        .filter(|event| matches!(event, SdkEvent::AgentSettled { .. }))
        .count();
    assert_eq!(
        settled_count, 1,
        "exactly one terminal event even after drops: {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(SdkEvent::AgentSettled { aborted: false })
        ),
        "the terminal event is delivered and is last: {:?}",
        events.last()
    );
    assert!(
        !events
            .iter()
            .skip_while(|event| !matches!(event, SdkEvent::AgentSettled { .. }))
            .skip(1)
            .any(|event| !matches!(event, SdkEvent::AgentSettled { .. })),
        "nothing follows the terminal event: {events:?}"
    );

    let mut seen = Vec::new();
    for event in &events {
        if let SdkEvent::MessageUpdate {
            delta,
            thinking: false,
        } = event
        {
            let index: usize = delta[1..4]
                .parse()
                .unwrap_or_else(|_| panic!("unexpected delta format: {delta:?}"));
            seen.push(index);
        }
    }
    assert!(
        !seen.is_empty(),
        "the consumer still observes some intermediate events: {events:?}"
    );
    assert!(
        seen.len() < DELTAS,
        "a capacity-2 channel behind a slow consumer must drop deltas: {seen:?}"
    );
    assert!(
        seen.windows(2).all(|window| window[0] < window[1]),
        "surviving deltas keep provider order: {seen:?}"
    );

    // The stream settled cleanly: the agent is usable for a non-streaming run.
    let outcome = agent
        .prompt("after the flood")
        .await
        .expect("prompt after a slow stream drain");
    assert!(
        outcome
            .message_text()
            .is_some_and(|text| text.ends_with("d299;")),
        "the run after the drained stream completes normally"
    );

    agent.shutdown().await.expect("shutdown");
}

// Round-13 foreground `on_event` + resource-loader tests (see the file header).
include!("sdk_round13_on_event.rs");

/// Round-15 A: the low-level `create_agent_session` factory keeps upstream
/// `createAgentSession` CLI parity — an unset `resource_loader` resolves an
/// environment `DefaultResourceLoader` and installs discovered skills. An
/// explicit loader replaces it (pinned/hermetic). No behavior change: the
/// docs and the option comment used to claim "None installs no prompt
/// resources", which contradicted upstream `core/sdk.ts:194-206`.
#[tokio::test]
async fn create_agent_session_default_loader_installs_ambient_skills_and_explicit_loader_replaces_it(
) {
    let _env_lock = lock_prompt_env();
    let _prompt_env = PromptEnvGuard::remove();
    let root = TempDir::new("sdk-low-level-resource-loader");
    let workspace = root.path().join("proj");
    let agent_dir = root.path().join("agent");
    let skill = agent_dir.join("skills").join("low-level-default-skill");
    std::fs::create_dir_all(&skill).expect("create skill dir");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: low-level-default-skill\ndescription: ambient default loader\n---\nBody\n",
    )
    .expect("write SKILL.md");

    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |_index, context| {
        recorded.lock().unwrap().push(context.clone());
        assistant(vec![text_block("ok")], StopReason::Stop)
    }));
    let created = pir::sdk::create_agent_session(CreateAgentSessionOptions {
        cwd: Some(workspace.clone()),
        agent_dir: Some(agent_dir.clone()),
        // Hermetic: pin the default loader's home instead of mutating the
        // process-wide HOME/USERPROFILE (parallel tests would race on it).
        home: Some(root.path().to_path_buf()),
        model_registry: Some(ModelRegistry::from_registry(Arc::clone(&registry))),
        model: Some(mock_model()),
        thinking_level: Some(ThinkingLevel::Off),
        no_session: true,
        ..Default::default()
    })
    .await
    .expect("create default-loader session");
    assert!(
        created
            .resources
            .skills
            .iter()
            .any(|skill| skill.name == "low-level-default-skill"),
        "an unset resource_loader must resolve the environment loader (CLI parity), got {:?}",
        created
            .resources
            .skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect::<Vec<_>>()
    );
    created
        .session
        .prompt("default", Arc::new(|_event| {}))
        .await
        .expect("prompt default-loader session");
    let system = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .and_then(|context| context.system_prompt)
        .expect("default-loader request carries a system prompt");
    assert!(
        system.contains("<available_skills>") && system.contains("low-level-default-skill"),
        "the default loader's skill must reach the derived prompt: {system}"
    );

    // Explicit loader with skills disabled: it replaces the environment
    // loader, so the same ambient skill must disappear.
    seen.lock().unwrap().clear();
    let explicit = pir::sdk::DefaultResourceLoader::new(workspace.clone(), agent_dir.clone())
        .with_home(root.path().to_path_buf())
        .without_skills()
        .without_prompt_templates();
    let created = pir::sdk::create_agent_session(CreateAgentSessionOptions {
        cwd: Some(workspace.clone()),
        agent_dir: Some(agent_dir.clone()),
        model_registry: Some(ModelRegistry::from_registry(registry)),
        model: Some(mock_model()),
        thinking_level: Some(ThinkingLevel::Off),
        no_session: true,
        resource_loader: Some(explicit),
        ..Default::default()
    })
    .await
    .expect("create explicit-loader session");
    assert!(
        created.resources.skills.is_empty(),
        "an explicit loader must replace the default, got {:?}",
        created
            .resources
            .skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect::<Vec<_>>()
    );
    created
        .session
        .prompt("explicit", Arc::new(|_event| {}))
        .await
        .expect("prompt explicit-loader session");
    let system = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .and_then(|context| context.system_prompt)
        .expect("explicit-loader request carries a system prompt");
    assert!(
        !system.contains("low-level-default-skill") && !system.contains("<available_skills>"),
        "the explicit loader must suppress the ambient skill: {system}"
    );
}

/// Round-15 B: a ScriptedProvider can hand the loop a structurally retryable
/// terminal error (`AiError::Retryable` + `attach_turn_retry_class`), and the
/// real agent-level retry policy (settings `retry`, NOT the transport budget)
/// re-issues the turn: one `AutoRetryStart`/`AutoRetryEnd(success)`, the
/// recovered outcome/usage, and the session JSONL contain the final answer
/// while the failed attempt is not persisted as transcript. A second run with
/// the same error every time exhausts the budget and reports the final error.
#[tokio::test]
async fn agent_retry_recovers_then_exhausts_with_auto_retry_events_usage_and_jsonl() {
    let root = TempDir::new("sdk-retry");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"retry":{"enabled":true,"maxRetries":1,"baseDelayMs":1,"maxAgentDelayMs":1}}"#,
    )
    .expect("write retry settings");

    let retryable = || {
        let mut message = assistant(Vec::new(), StopReason::Error);
        message.error_message = Some("429 Too Many Requests: rate limit".to_owned());
        message.usage = Usage {
            input: 7,
            output: 0,
            total_tokens: 7,
            ..Usage::default()
        };
        pir::ai::error::attach_turn_retry_class(
            &mut message,
            &pir::ai::error::AiError::Retryable("429 Too Many Requests: rate limit".to_owned()),
        );
        message
    };

    // --- Recover on the second attempt -------------------------------------
    let started: Arc<Mutex<Vec<(u32, u32, u64, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let ended: Arc<Mutex<Vec<(bool, u32, Option<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_provider = Arc::clone(&calls);
    let registry_recover = registry(Arc::new(move |index, _context| {
        calls_for_provider.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            retryable()
        } else {
            let mut message = assistant(vec![text_block("recovered")], StopReason::Stop);
            message.usage = Usage {
                input: 5,
                output: 3,
                total_tokens: 8,
                ..Usage::default()
            };
            message
        }
    }));
    let agent = agent_builder(registry_recover, root.path())
        .agent_dir(agent_dir.clone())
        .session(SessionMode::Dir(root.path().join("sessions")))
        .build()
        .await
        .expect("build retrying agent");
    let started_sink = Arc::clone(&started);
    let ended_sink = Arc::clone(&ended);
    agent.on_session_event(move |event| match event {
        pir::sdk::SessionEvent::AutoRetryStart {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => started_sink
            .lock()
            .unwrap()
            .push((attempt, max_attempts, delay_ms, error_message)),
        pir::sdk::SessionEvent::AutoRetryEnd {
            success,
            attempt,
            final_error,
        } => ended_sink
            .lock()
            .unwrap()
            .push((success, attempt, final_error)),
        _ => {}
    });

    let outcome = agent.prompt("retry me").await.expect("prompt recovers");
    assert_eq!(outcome.message.stop_reason, StopReason::Stop);
    assert_eq!(outcome.message_text().as_deref(), Some("recovered"));
    assert_eq!(outcome.usage.total_tokens, 8, "terminal usage only");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "one failed attempt + one retry"
    );
    {
        let started = started.lock().unwrap();
        assert_eq!(started.len(), 1, "one scheduled retry: {started:?}");
        assert_eq!(started[0].0, 1);
        assert_eq!(started[0].1, 1);
        assert_eq!(started[0].2, 1);
        assert!(
            started[0].3.contains("429"),
            "the retry trigger carries the provider error: {:?}",
            started[0].3
        );
    }
    assert_eq!(
        *ended.lock().unwrap(),
        vec![(true, 1, None)],
        "a recovered retry closes with success and no final error"
    );
    assert_eq!(
        agent.usage_totals().total_tokens,
        8,
        "session totals reflect the terminal settled message (the failed attempt's usage \
         stays on the loop's run-total path, not the persisted session counter)"
    );

    let session_path = agent.session_path().expect("session JSONL path");
    let jsonl = std::fs::read_to_string(&session_path).expect("read session JSONL");
    assert!(
        jsonl.contains("recovered"),
        "the recovered answer is persisted: {jsonl}"
    );
    assert!(
        !jsonl.contains("rate limit"),
        "the failed attempt is not persisted as transcript: {jsonl}"
    );
    agent.shutdown().await.expect("shutdown recovered agent");

    // --- Exhaust the same budget -------------------------------------------
    let started: Arc<Mutex<Vec<(u32, u32, u64, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let ended: Arc<Mutex<Vec<(bool, u32, Option<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_provider = Arc::clone(&calls);
    let registry_exhaust = registry(Arc::new(move |_index, _context| {
        calls_for_provider.fetch_add(1, Ordering::SeqCst);
        retryable()
    }));
    let exhausted = agent_builder(registry_exhaust, root.path())
        .agent_dir(agent_dir)
        .no_session()
        .build()
        .await
        .expect("build always-failing agent");
    let started_sink = Arc::clone(&started);
    let ended_sink = Arc::clone(&ended);
    exhausted.on_session_event(move |event| match event {
        pir::sdk::SessionEvent::AutoRetryStart {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => started_sink
            .lock()
            .unwrap()
            .push((attempt, max_attempts, delay_ms, error_message)),
        pir::sdk::SessionEvent::AutoRetryEnd {
            success,
            attempt,
            final_error,
        } => ended_sink
            .lock()
            .unwrap()
            .push((success, attempt, final_error)),
        _ => {}
    });
    let outcome = exhausted
        .prompt("always fails")
        .await
        .expect("an exhausted retry still returns the terminal message");
    assert_eq!(outcome.message.stop_reason, StopReason::Error);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "1 initial + 1 retry");
    assert_eq!(started.lock().unwrap().len(), 1);
    {
        let ended = ended.lock().unwrap();
        assert_eq!(ended.len(), 1, "exactly one auto_retry_end: {ended:?}");
        assert!(!ended[0].0, "the retry failed");
        assert_eq!(ended[0].1, 1);
        assert!(
            ended[0]
                .2
                .as_deref()
                .is_some_and(|error| error.contains("429")),
            "the final error is the provider's text: {:?}",
            ended[0].2
        );
    }
    exhausted
        .shutdown()
        .await
        .expect("shutdown exhausted agent");
}

// ---------------------------------------------------------------------------
// Round 17 — custom AgentTool full context (closure + typed)
// ---------------------------------------------------------------------------

/// Typed tool exercising every `ToolContext` surface a host-backed tool can
/// rely on: cwd, abort flag, session environment, the live update channel,
/// and nested `executeTool`.
struct ContextProbeTool {
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl AgentTool for ContextProbeTool {
    fn name(&self) -> &'static str {
        "context_probe"
    }

    fn description(&self) -> &'static str {
        "Report the SDK tool context"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({
            "type": "object",
            "properties": { "note": { "type": "string" } }
        }))
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let note = args["note"].as_str().unwrap_or_default().to_owned();
        let cwd = ctx.cwd.display().to_string();
        let aborted = ctx.is_aborted();
        let env = ctx.session_env.as_ref().map(|env| {
            json!({
                "model": format!("{}/{}", env.model.provider, env.model.id),
                "session_id": env.session_id.clone(),
            })
        });
        let update = ctx.update_channel();
        let nested = ctx.nested.clone();
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            if let Some(update) = &update {
                update(json!({ "progress": 1, "note": note }));
            }
            let nested_text = match nested.as_deref() {
                Some(executor) => {
                    let outcome = executor
                        .execute("word_count", json!({ "text": "one two three" }))
                        .await;
                    format!(
                        "{}:{}:{}",
                        outcome.tool_call_id, outcome.result.content, outcome.is_error
                    )
                }
                None => "nested-unavailable".to_owned(),
            };
            if let Some(update) = &update {
                update(json!({ "progress": 2 }));
            }
            seen.lock().unwrap().push(json!({
                "cwd": cwd,
                "aborted": aborted,
                "session_env": env,
                "nested": nested_text,
            }));
            ToolResult::ok(format!(
                "cwd={cwd}; aborted={aborted}; nested={nested_text}"
            ))
        })
    }
}

#[tokio::test]
async fn custom_closure_and_typed_tools_receive_documented_context_and_results_flow_back() {
    let temp = TempDir::new("custom-tool-context");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let requests: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let registry = registry(Arc::new(move |index, context| {
        recorded.lock().unwrap().push(context.clone());
        match index {
            0 => assistant(
                vec![
                    // Missing `text` -> closure-level parameter error.
                    tool_call_block("call-1", "echo_checked", vec![]),
                    // Empty `text` -> second validation branch.
                    tool_call_block("call-2", "echo_checked", vec![("text", json!("  "))]),
                    // Typed tool: full ctx + nested executeTool.
                    tool_call_block("call-3", "context_probe", vec![("note", json!("probe"))]),
                ],
                StopReason::ToolUse,
            ),
            _ => assistant(vec![text_block("done")], StopReason::Stop),
        }
    }));
    let agent = agent_builder(registry, temp.path())
        .agent_dir(temp.path().join("agent"))
        .session(SessionMode::Dir(temp.path().join("sessions")))
        .with_tool(Arc::new(WordCountTool))
        .with_tool(Arc::new(ContextProbeTool {
            seen: Arc::clone(&seen),
        }))
        .tool(
            "echo_checked",
            "Echo text after validating the argument",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
            |args| async move {
                match args["text"].as_str() {
                    None => ToolResult::error("missing required argument: text"),
                    Some(text) if text.trim().is_empty() => {
                        ToolResult::error("text must not be empty")
                    }
                    Some(text) => ToolResult::ok(format!("echo:{text}")),
                }
            },
        )
        .build()
        .await
        .expect("build agent");

    let events: Vec<SdkEvent> = agent.prompt_stream("run the custom tools").collect().await;

    let mut starts: Vec<(String, String, Option<String>)> = Vec::new();
    let mut ends: Vec<(String, String, bool, Option<String>)> = Vec::new();
    let mut updates: Vec<(String, serde_json::Value)> = Vec::new();
    for event in &events {
        match event {
            SdkEvent::ToolExecutionStart {
                call_id,
                tool_name,
                parent_call_id,
                ..
            } => starts.push((call_id.clone(), tool_name.clone(), parent_call_id.clone())),
            SdkEvent::ToolExecutionEnd {
                call_id,
                tool_name,
                is_error,
                parent_call_id,
                ..
            } => ends.push((
                call_id.clone(),
                tool_name.clone(),
                *is_error,
                parent_call_id.clone(),
            )),
            SdkEvent::ToolExecutionUpdate {
                tool_call_id,
                partial,
                ..
            } => updates.push((tool_call_id.clone(), partial.clone())),
            _ => {}
        }
    }

    // Source-order starts, including the nested call scoped to call-3.
    assert_eq!(
        starts
            .iter()
            .map(|(id, name, parent)| (id.as_str(), name.as_str(), parent.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            ("call-1", "echo_checked", None),
            ("call-2", "echo_checked", None),
            ("call-3", "context_probe", None),
            ("call-3/0", "word_count", Some("call-3")),
        ],
        "tool execution starts must include the nested executeTool call"
    );
    let end = |id: &str| {
        ends.iter()
            .find(|(call_id, _, _, _)| call_id == id)
            .unwrap_or_else(|| panic!("missing tool_execution_end for {id}: {ends:?}"))
    };
    assert!(end("call-1").2, "missing argument is an error result");
    assert!(end("call-2").2, "empty argument is an error result");
    assert!(!end("call-3").2, "typed probe succeeds");
    assert_eq!(
        end("call-3/0").3.as_deref(),
        Some("call-3"),
        "nested end carries parentToolCallId"
    );

    // update_channel reached the event stream with both progress snapshots,
    // EXACTLY once each and in source order (PIR-SDK-006: the loop projection
    // and the plugin-host bridge must not both deliver the same update).
    let updates_for_call: Vec<serde_json::Value> = updates
        .iter()
        .filter(|(id, _)| id == "call-3")
        .map(|(_, partial)| partial.clone())
        .collect();
    assert_eq!(
        updates_for_call,
        vec![
            json!({"progress": 1, "note": "probe"}),
            json!({"progress": 2})
        ],
        "each tool update must be delivered exactly once with the clean tool-defined partial"
    );
    assert!(
        updates_for_call
            .iter()
            .all(|partial| partial.get("toolCallId").is_none()),
        "scheduler identity stamps must not leak into the documented partial"
    );

    // Typed ctx: cwd, abort state, session env, nested result.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "one context probe ran");
    let probe = &seen[0];
    assert_eq!(
        std::fs::canonicalize(probe["cwd"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(temp.path()).unwrap(),
        "ctx.cwd is the builder cwd"
    );
    assert_eq!(probe["aborted"], json!(false));
    assert_eq!(
        probe["session_env"]["model"],
        json!("scripted/scripted-model")
    );
    assert_eq!(
        probe["session_env"]["session_id"].as_str(),
        agent.session_id().as_deref(),
        "ctx.session_env carries the live session id"
    );
    assert_eq!(
        probe["nested"],
        json!("call-3/0:3 words:false"),
        "nested executeTool ran through the scheduler"
    );
    drop(seen);

    // Results flow into the next model request as toolResult messages.
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "tool round-trip then final answer");
    let next_round: Vec<(String, String, bool)> = requests[1]
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some((
                result.tool_call_id.clone(),
                result.tool_name.clone(),
                result.is_error,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        next_round,
        vec![
            ("call-1".into(), "echo_checked".into(), true),
            ("call-2".into(), "echo_checked".into(), true),
            ("call-3".into(), "context_probe".into(), false),
        ]
    );
    drop(requests);

    // Persisted session JSONL carries the same results. The missing-argument
    // call is rejected by the shared schema validator before `run` (so the
    // closure's None branch is only defensive); the empty string reaches the
    // closure and returns its own ToolResult::error.
    let session_path = agent.session_path().expect("persistent session path");
    let persisted = std::fs::read_to_string(&session_path).expect("read session file");
    for needle in [
        "echo_checked",
        "context_probe",
        "missing required property 'text'",
        "text must not be empty",
    ] {
        assert!(
            persisted.contains(needle),
            "session JSONL lacks {needle}: {persisted}"
        );
    }
}

/// A typed tool that records how many calls are inside `run` at once.
struct ParallelProbeTool {
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
}

impl AgentTool for ParallelProbeTool {
    fn name(&self) -> &'static str {
        "parallel_probe"
    }

    fn description(&self) -> &'static str {
        "Sleep briefly and report batch concurrency"
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let in_flight = Arc::clone(&self.in_flight);
        let max_in_flight = Arc::clone(&self.max_in_flight);
        let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        max_in_flight.fetch_max(current, Ordering::SeqCst);
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            ToolResult::ok("probe done")
        })
    }
}

/// A typed tool that polls `ToolContext::is_aborted()` and returns an error
/// once the run is cancelled.
struct AbortWatchTool {
    entered: Arc<AtomicUsize>,
}

impl AgentTool for AbortWatchTool {
    fn name(&self) -> &'static str {
        "abort_watch"
    }

    fn description(&self) -> &'static str {
        "Wait until the abort signal fires"
    }

    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let ctx = ctx.clone();
        Box::pin(async move {
            loop {
                if ctx.is_aborted() {
                    return ToolResult::error("aborted by ToolContext::is_aborted");
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
    }
}

#[tokio::test]
async fn typed_tools_run_in_parallel_batches_and_see_the_abort_signal() {
    let temp = TempDir::new("typed-parallel-abort");
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = registry(Arc::new(|index, _context| match index {
        0 => assistant(
            vec![
                tool_call("call-par-1", "parallel_probe"),
                tool_call("call-par-2", "parallel_probe"),
            ],
            StopReason::ToolUse,
        ),
        1 => assistant(vec![text_block("parallel done")], StopReason::Stop),
        2 => assistant(
            vec![tool_call("call-abort", "abort_watch")],
            StopReason::ToolUse,
        ),
        _ => assistant(vec![text_block("unexpected")], StopReason::Stop),
    }));
    let agent = agent_builder(registry, temp.path())
        .no_session()
        .with_tool(Arc::new(ParallelProbeTool {
            in_flight: Arc::clone(&in_flight),
            max_in_flight: Arc::clone(&max_in_flight),
        }))
        .with_tool(Arc::new(AbortWatchTool {
            entered: Arc::clone(&entered),
        }))
        .build()
        .await
        .expect("build agent");

    let outcome = agent
        .prompt("run both probes")
        .await
        .expect("parallel prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("parallel done"));
    assert_eq!(
        max_in_flight.load(Ordering::SeqCst),
        2,
        "both calls of one batch must be inside run() at the same time"
    );

    let drain_task = tokio::spawn({
        let agent = agent.clone();
        async move {
            let mut events = Box::pin(agent.prompt_stream_with_outcome("cancel this run"));
            let mut settled = None;
            let mut outcome = None;
            while let Some(event) = events.as_mut().next().await {
                match event {
                    SdkEvent::AgentOutcome(value) => outcome = Some(value),
                    SdkEvent::AgentSettled { aborted } => {
                        settled = Some(aborted);
                        break;
                    }
                    _ => {}
                }
            }
            (settled, outcome)
        }
    });
    wait_for_tool_entry(&entered).await;
    agent.abort();
    let (settled, outcome) = drain_task.await.expect("drain join");
    assert_eq!(settled, Some(true), "the stream settles as aborted");
    let outcome = outcome.expect("aborted run still reports an outcome");
    assert!(outcome.aborted, "the run is reported as aborted");
    let aborted_result = outcome
        .tool_results
        .iter()
        .find(|result| result.tool_name == "abort_watch")
        .expect("the aborted call has a tool result");
    assert!(aborted_result.is_error);
    assert_eq!(
        content_text(&aborted_result.content),
        "aborted by ToolContext::is_aborted"
    );
}

// ---------------------------------------------------------------------------
// Round 17 — inline plugin: tool, slash command, event handler + builtin todo
// ---------------------------------------------------------------------------

struct PluginEchoTool;

impl AgentTool for PluginEchoTool {
    fn name(&self) -> &'static str {
        "plugin_echo"
    }

    fn description(&self) -> &'static str {
        "Echo text from the inline plugin"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }))
    }

    fn run(&self, args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let text = args["text"].as_str().unwrap_or_default().to_owned();
        Box::pin(async move { ToolResult::ok(format!("plugin says {text}")) })
    }
}

struct AuditPlugin {
    info: PluginInfo,
    events: Arc<Mutex<Vec<String>>>,
    command_args: Arc<Mutex<Vec<String>>>,
}

impl AgentPlugin for AuditPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(PluginEchoTool));
        let command_args = Arc::clone(&self.command_args);
        let handler: CommandHandler = Arc::new(move |args: &str| {
            let entry = format!("audit:{args}");
            let display = format!("plugin command ran with args={args}");
            let log = Arc::clone(&command_args);
            Box::pin(async move {
                log.lock().unwrap().push(entry);
                pir::extensions::CommandOutcome {
                    ok: true,
                    error: Some(display),
                }
            })
        });
        ctx.register_command(
            "audit",
            Some("Run the inline plugin audit command"),
            Some(CommandBackend::Rust(handler)),
        );
        Ok(())
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(
            ["input", "before_agent_start", "tool_call", "tool_result"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        self.events.lock().unwrap().push(event.name.to_string());
        Box::pin(async { PluginEventOutcome::None })
    }
}

#[tokio::test]
async fn inline_plugin_registers_tool_command_and_events_alongside_builtin_todo() {
    let temp = TempDir::new("inline-plugin");
    let events = Arc::new(Mutex::new(Vec::new()));
    let command_args = Arc::new(Mutex::new(Vec::new()));
    let registry = registry(Arc::new(|index, _context| match index {
        0 => assistant(
            vec![
                tool_call_block("call-1", "plugin_echo", vec![("text", json!("hello"))]),
                tool_call_block(
                    "call-2",
                    "todo",
                    vec![
                        ("action", json!("create")),
                        ("subject", json!("plugin task")),
                    ],
                ),
            ],
            StopReason::ToolUse,
        ),
        _ => assistant(vec![text_block("plugin done")], StopReason::Stop),
    }));
    let agent = agent_builder(registry, temp.path())
        .agent_dir(temp.path().join("agent"))
        .session(SessionMode::Dir(temp.path().join("sessions")))
        .with_plugin(Arc::new(AuditPlugin {
            info: PluginInfo::inline("audit-plugin"),
            events: Arc::clone(&events),
            command_args: Arc::clone(&command_args),
        }))
        .builtin("todo")
        .build()
        .await
        .expect("build agent with inline plugin and todo builtin");

    let tool_names: Vec<String> = agent.tools().into_iter().map(|tool| tool.name).collect();
    assert!(
        tool_names.iter().any(|name| name == "plugin_echo"),
        "inline plugin tool is in the unified registry: {tool_names:?}"
    );
    assert!(
        tool_names.iter().any(|name| name == "todo"),
        "builtin todo tool coexists with the plugin tool: {tool_names:?}"
    );

    let commands = agent.commands();
    assert!(
        commands.iter().any(|(name, _)| name == "audit"),
        "inline plugin command is listed: {commands:?}"
    );
    assert!(
        commands.iter().any(|(name, _)| name == "todos"),
        "builtin todo command is listed: {commands:?}"
    );

    let outcome = agent.prompt("use the inline plugin").await.expect("prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("plugin done"));
    let results = outcome.tool_result_texts().join("\n");
    assert!(
        results.contains("plugin says hello"),
        "plugin tool ran: {results}"
    );
    assert!(
        results.contains("plugin task"),
        "todo builtin ran: {results}"
    );

    let log = events.lock().unwrap().clone();
    assert_eq!(
        log.first().map(String::as_str),
        Some("input"),
        "the plugin observes `input` first: {log:?}"
    );
    assert_eq!(
        log.get(1).map(String::as_str),
        Some("before_agent_start"),
        "the plugin observes `before_agent_start` before the model runs: {log:?}"
    );
    assert_eq!(
        log.iter()
            .filter(|name| name.as_str() == "tool_call")
            .count(),
        2,
        "one tool_call event per batch member: {log:?}"
    );
    assert_eq!(
        log.iter()
            .filter(|name| name.as_str() == "tool_result")
            .count(),
        2,
        "one tool_result event per batch member: {log:?}"
    );
    let first_result = log
        .iter()
        .position(|name| name == "tool_result")
        .expect("a tool result event");
    let last_call = log
        .iter()
        .rposition(|name| name == "tool_call")
        .expect("a tool call event");
    assert!(
        last_call < first_result,
        "every tool_call precedes the tool_result phase: {log:?}"
    );

    let command = agent
        .dispatch_command("audit", "hello world")
        .await
        .expect("dispatch inline plugin command");
    assert!(command.ok);
    assert_eq!(
        command.error.as_deref(),
        Some("plugin command ran with args=hello world")
    );
    assert_eq!(
        command_args.lock().unwrap().as_slice(),
        ["audit:hello world".to_owned()]
    );

    let todos = agent
        .dispatch_command("todos", "")
        .await
        .expect("dispatch builtin todo command");
    // Headless SDK sessions have no UI: the builtin /todos command is
    // registered and dispatched through the same plugin host, and reports its
    // documented interactive-only gate instead of touching the store.
    assert!(!todos.ok);
    assert!(
        todos
            .error
            .as_deref()
            .is_some_and(|text| text.to_lowercase().contains("interactive")),
        "the todo command reports its headless gate: {:?}",
        todos.error
    );
}

// ---------------------------------------------------------------------------
// Round 18 — PIR-SDK-006: one ToolExecutionUpdate per tool-emitted update
// ---------------------------------------------------------------------------

/// Typed tool that emits three live progress snapshots through the documented
/// `ToolContext::update_channel`.
struct UpdateProbeTool;

impl AgentTool for UpdateProbeTool {
    fn name(&self) -> &'static str {
        "update_probe"
    }

    fn description(&self) -> &'static str {
        "Emit three live progress snapshots"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({ "type": "object" }))
    }

    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let update = ctx.update_channel();
        Box::pin(async move {
            if let Some(update) = &update {
                for progress in 1..=3 {
                    update(json!({ "progress": progress }));
                }
            }
            ToolResult::ok("probe done")
        })
    }
}

/// PIR-SDK-006 regression: every tool-emitted update is delivered EXACTLY
/// once, whether it comes from a typed `update_channel`, a real foreground
/// bash streaming snapshots, or a detached `background: true` bash. The loop
/// projection is authoritative for updates it observes; the plugin-host
/// bridge must only fill in updates that have no loop counterpart.
#[tokio::test]
async fn tool_updates_are_delivered_exactly_once_for_typed_foreground_and_background_tools() {
    let cwd = TempDir::new("sdk-update-dedup");
    let sessions = cwd.path().join("sessions");
    let registry = registry(Arc::new(|index, _context| match index {
        0 => assistant(
            vec![
                tool_call_block("upd-1", "update_probe", vec![]),
                tool_call_block(
                    "bash-1",
                    "bash",
                    vec![(
                        "command",
                        json!("printf 'tick-a\n'; sleep 0.35; printf 'tick-b\n'"),
                    )],
                ),
                tool_call_block(
                    "bg-1",
                    "bash",
                    vec![
                        ("command", json!("printf bg-tick; sleep 0.5")),
                        ("background", json!(true)),
                    ],
                ),
            ],
            StopReason::ToolUse,
        ),
        _ => assistant(vec![text_block("done")], StopReason::Stop),
    }));
    let agent = agent_builder(registry, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Dir(sessions))
        .core_tools(true)
        .with_tool(Arc::new(UpdateProbeTool))
        .build()
        .await
        .expect("build update-probe agent");

    let events: Vec<SdkEvent> = agent.prompt_stream("exercise live updates").collect().await;
    let updates: Vec<(String, serde_json::Value)> = events
        .iter()
        .filter_map(|event| match event {
            SdkEvent::ToolExecutionUpdate {
                tool_call_id,
                partial,
                ..
            } => Some((tool_call_id.clone(), partial.clone())),
            _ => None,
        })
        .collect();

    // The core invariant: no (call id, partial) pair may appear twice.
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (id, partial) in &updates {
        *counts.entry(format!("{id}\u{1f}{partial}")).or_default() += 1;
    }
    let duplicates: Vec<(&String, &usize)> =
        counts.iter().filter(|(_, count)| **count != 1).collect();
    assert!(
        duplicates.is_empty(),
        "each tool update must be delivered exactly once, duplicates: {duplicates:?}; all: {updates:?}"
    );

    // Typed tool: all three snapshots, in source order, clean partials.
    let probe: Vec<serde_json::Value> = updates
        .iter()
        .filter(|(id, _)| id == "upd-1")
        .map(|(_, partial)| partial.clone())
        .collect();
    assert_eq!(
        probe,
        vec![
            json!({ "progress": 1 }),
            json!({ "progress": 2 }),
            json!({ "progress": 3 }),
        ],
        "typed update_channel snapshots must be exactly once and in source order"
    );

    // Real foreground bash streams live snapshots through the same channel.
    let bash_updates = updates.iter().filter(|(id, _)| id == "bash-1").count();
    assert!(
        bash_updates >= 1,
        "real foreground bash must stream at least its initial snapshot: {updates:?}"
    );
    // Detached background bash also reports through the update channel.
    let bg_updates = updates.iter().filter(|(id, _)| id == "bg-1").count();
    assert!(
        bg_updates >= 1,
        "detached background bash must stream at least its initial snapshot: {updates:?}"
    );

    agent.shutdown().await.expect("shutdown update-probe agent");
}

// ---------------------------------------------------------------------------
// Round 18 — extension_paths + only_builtin(true) composition
// ---------------------------------------------------------------------------

/// `only_builtin(true)` reduces `extension_paths` to `builtin:<name>` entries:
/// an on-disk JS extension is ignored (no QuickJS actor) while the requested
/// builtin still registers its native tool/command surface.
#[tokio::test]
async fn only_builtin_ignores_extension_paths_and_still_registers_requested_builtins() {
    let temp = TempDir::new("sdk-only-builtin");
    let js = temp.path().join("ignored_extension.js");
    std::fs::write(
        &js,
        r#"
        export default function (pi) {
          pi.registerTool({
            name: "js_probe",
            description: "tool from an on-disk extension",
            parameters: { type: "object" },
            execute: async () => ({ content: [{ type: "text", text: "js" }] })
          });
        }
        "#,
    )
    .expect("write extension fixture");
    let ok_registry = || {
        registry(Arc::new(|_i, _c| {
            assistant(vec![text_block("ok")], StopReason::Stop)
        }))
    };

    // Positive control: without only_builtin the same file loads and its tool
    // is visible, so the absence below is due to the policy, not a bad fixture.
    let loaded = agent_builder(ok_registry(), temp.path())
        .agent_dir(temp.path().join("agent-loaded"))
        .extension_paths(vec![js.clone()])
        .build()
        .await
        .expect("on-disk extension loads without only_builtin");
    assert!(
        loaded.tools().iter().any(|tool| tool.name == "js_probe"),
        "positive control: the JS extension's tool is registered"
    );
    loaded.shutdown().await.expect("shutdown loaded agent");

    let agent = agent_builder(ok_registry(), temp.path())
        .agent_dir(temp.path().join("agent-only-builtin"))
        .extension_paths(vec![js, PathBuf::from("builtin:todo")])
        .only_builtin(true)
        .build()
        .await
        .expect("only_builtin build ignores the on-disk path");
    let tool_names: Vec<String> = agent.tools().into_iter().map(|tool| tool.name).collect();
    assert!(
        tool_names.iter().any(|name| name == "todo"),
        "the requested builtin:todo still loads: {tool_names:?}"
    );
    assert!(
        !tool_names.iter().any(|name| name == "js_probe"),
        "the on-disk extension must be ignored: {tool_names:?}"
    );
    let commands: Vec<String> = agent.commands().into_iter().map(|(name, _)| name).collect();
    assert!(
        commands.iter().any(|name| name == "todos"),
        "the builtin command surface is registered: {commands:?}"
    );
    agent.shutdown().await.expect("shutdown only_builtin agent");
}

// ---------------------------------------------------------------------------
// Round 19 — low-level factory vs builder only_builtin parity, strict failures
// ---------------------------------------------------------------------------

/// Task A: the low-level `create_agent_session` factory has no `only_builtin`
/// switch (its `extensions` field takes an already-loaded handle, matching the
/// upstream `CreateAgentSessionOptions` shape). Parity therefore means: the
/// same public filter ([`pir::plugins::filter_builtin_paths`]) plus a strict
/// plan-only load yields exactly the surface `AgentBuilder::only_builtin(true)`
/// produces for the same `[ignored.js, builtin:todo]` request — same tools,
/// same native commands, no on-disk JS in either path.
#[tokio::test]
async fn low_level_create_agent_session_matches_builder_only_builtin_plan() {
    let temp = TempDir::new("sdk-only-builtin-parity");
    let js = temp.path().join("ignored_extension.js");
    std::fs::write(
        &js,
        r#"
        export default function (pi) {
          pi.registerTool({
            name: "js_probe",
            description: "tool from an on-disk extension",
            parameters: { type: "object" },
            execute: async () => ({ content: [{ type: "text", text: "js" }] })
          });
        }
        "#,
    )
    .expect("write extension fixture");
    let respond = || {
        registry(Arc::new(|_index, _context| {
            assistant(vec![text_block("ok")], StopReason::Stop)
        }))
    };

    // Builder path.
    let builder_agent = agent_builder(respond(), temp.path())
        .agent_dir(temp.path().join("agent-builder"))
        .extension_paths(vec![js.clone(), PathBuf::from("builtin:todo")])
        .only_builtin(true)
        .build()
        .await
        .expect("builder only_builtin build");
    let mut builder_tools: Vec<String> = builder_agent
        .tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    builder_tools.sort();
    let mut builder_commands: Vec<String> = builder_agent
        .commands()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    builder_commands.sort();
    builder_agent
        .shutdown()
        .await
        .expect("shutdown builder agent");

    // Low-level path: apply the same policy through the public helpers, then
    // hand the plan-only handle to the factory.
    let (kept, ignored) =
        pir::plugins::filter_builtin_paths(&[js.clone(), PathBuf::from("builtin:todo")]);
    assert_eq!(ignored, vec![js.clone()], "the on-disk path is ignored");
    assert_eq!(kept, vec![PathBuf::from("builtin:todo")]);
    let (handle, failures) = pir::agent_session::load_extension_runtime_strict(
        &kept,
        pir::extensions::ExtensionConfig {
            mode: "headless".to_owned(),
            cwd: temp.path().to_path_buf(),
            ..Default::default()
        },
    )
    .await
    .expect("plan-only strict load");
    assert!(failures.is_empty(), "builtin-only plan loads: {failures:?}");
    let handle = handle.expect("builtin-only plan yields a plan-only handle");
    let agent_dir = temp.path().join("agent-low");
    let created = pir::sdk::create_agent_session(CreateAgentSessionOptions {
        cwd: Some(temp.path().to_path_buf()),
        agent_dir: Some(agent_dir.clone()),
        model_registry: Some(ModelRegistry::from_registry(respond())),
        model: Some(mock_model()),
        thinking_level: Some(ThinkingLevel::Off),
        no_session: true,
        no_tools: true,
        extensions: Some(handle),
        resource_loader: Some(
            DefaultResourceLoader::new(temp.path().to_path_buf(), agent_dir.clone())
                .with_home(temp.path().to_path_buf())
                .without_skills()
                .without_prompt_templates(),
        ),
        ..Default::default()
    })
    .await
    .expect("low-level build with plan-only handle");
    let host = created.session.plugin_host().expect("plugin host");
    let mut low_tools: Vec<String> = host
        .registry()
        .resolved_tools()
        .iter()
        .map(|(def, _)| def.name.clone())
        .collect();
    low_tools.sort();
    let mut low_commands: Vec<String> = host
        .registry()
        .resolved_commands()
        .iter()
        .map(|command| command.invocation_name.clone())
        .collect();
    low_commands.sort();

    assert_eq!(
        builder_tools, low_tools,
        "builder and low-level paths must register the same tools"
    );
    assert_eq!(
        builder_commands, low_commands,
        "builder and low-level paths must register the same native commands"
    );
    assert_eq!(
        builder_tools,
        vec!["todo".to_owned()],
        "only_builtin is the filter shape: no default builtin injection in either path"
    );
    assert!(
        !builder_tools.iter().any(|name| name == "js_probe"),
        "neither path may load the ignored on-disk extension: {builder_tools:?}"
    );
    assert!(
        host.has_handlers("session_start"),
        "the builtin's event surface rides the same plugin host: session_start registered"
    );
    created.session.shutdown().await;
}

/// Task B: `extension_paths` strict failure mapping.
///
/// - a syntax-broken JS file is an `SdkError::Extension` naming the path;
/// - an unknown `builtin:<name>` is an `SdkError::Other` with the upstream
///   unknown-name wording;
/// - a missing path is an `SdkError::Extension` naming the path;
/// - `only_builtin(true)` ignores a broken on-disk path instead of failing.
#[tokio::test]
async fn extension_paths_strict_failures_map_to_documented_errors_and_only_builtin_ignores_bad_files(
) {
    let temp = TempDir::new("sdk-extension-errors");
    let bad_js = temp.path().join("broken_extension.js");
    std::fs::write(&bad_js, "export default function ( {").expect("write broken fixture");
    let missing = temp.path().join("missing_extension.js");
    let respond = || {
        registry(Arc::new(|_index, _context| {
            assistant(vec![text_block("ok")], StopReason::Stop)
        }))
    };

    // 1. Bad JS syntax -> Extension, message carries the path.
    let error = agent_builder(respond(), temp.path())
        .agent_dir(temp.path().join("agent-bad-js"))
        .extension_paths(vec![bad_js.clone()])
        .build()
        .await
        .expect_err("broken JS must fail the build");
    match error {
        SdkError::Extension(message) => {
            assert!(
                message.contains(&bad_js.display().to_string()),
                "the extension error must name the offending path: {message}"
            );
        }
        other => panic!("broken JS must map to SdkError::Extension, got {other:?}"),
    }

    // 2. Unknown builtin -> Other with the upstream wording.
    let error = agent_builder(respond(), temp.path())
        .agent_dir(temp.path().join("agent-unknown-builtin"))
        .extension_paths(vec![PathBuf::from("builtin:pir-no-such-builtin")])
        .build()
        .await
        .expect_err("unknown builtin must fail the build");
    match error {
        SdkError::Other(message) => {
            assert!(
                message.contains("Unknown built-in extension"),
                "unknown builtin keeps the upstream wording: {message}"
            );
            assert!(
                message.contains("builtin:pir-no-such-builtin"),
                "the message names the requested builtin: {message}"
            );
        }
        other => panic!("unknown builtin must map to SdkError::Other, got {other:?}"),
    }

    // 3. Missing path -> Extension naming the path.
    let error = agent_builder(respond(), temp.path())
        .agent_dir(temp.path().join("agent-missing"))
        .extension_paths(vec![missing.clone()])
        .build()
        .await
        .expect_err("missing extension path must fail the build");
    match error {
        SdkError::Extension(message) => {
            assert!(
                message.contains(&missing.display().to_string()),
                "the extension error must name the missing path: {message}"
            );
        }
        other => panic!("missing path must map to SdkError::Extension, got {other:?}"),
    }

    // 4. only_builtin(true): the broken path is ignored before any load.
    let agent = agent_builder(respond(), temp.path())
        .agent_dir(temp.path().join("agent-only-builtin-bad"))
        .extension_paths(vec![bad_js, PathBuf::from("builtin:todo")])
        .only_builtin(true)
        .build()
        .await
        .expect("only_builtin ignores the broken on-disk path");
    let tools: Vec<String> = agent.tools().into_iter().map(|tool| tool.name).collect();
    assert!(
        tools.iter().any(|name| name == "todo"),
        "the requested builtin still loads: {tools:?}"
    );
    assert!(
        !tools.iter().any(|name| name == "js_probe"),
        "no on-disk extension surface: {tools:?}"
    );
    agent.shutdown().await.expect("shutdown bad-path agent");
}

// ---------------------------------------------------------------------------
// Round 20 — dedupe fingerprint set under a long streaming run
// ---------------------------------------------------------------------------

/// Typed tool that emits `updates` distinct live snapshots in one call, the
/// shape of a fast streaming tool (`ToolContext::update_channel`).
struct FloodProbeTool {
    updates: usize,
}

impl AgentTool for FloodProbeTool {
    fn name(&self) -> &'static str {
        "flood_probe"
    }

    fn description(&self) -> &'static str {
        "Emit a burst of live progress snapshots"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({ "type": "object" }))
    }

    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let updates = self.updates;
        let channel = ctx.update_channel();
        Box::pin(async move {
            if let Some(update) = &channel {
                for seq in 0..updates {
                    update(json!({ "seq": seq }));
                }
            }
            ToolResult::ok("flood done")
        })
    }
}

/// Round-20 stress: several thousand updates in ONE prompt run. The loop
/// projection must still deliver every (call id, partial) pair exactly once
/// (the bridge copy is skipped), and wall-clock latency must stay linear in
/// the update count. The dedupe set's per-entry expiry is unit-tested in
/// `src/sdk/event.rs` (`tool_update_dedupe_tests`); this exercises the real
/// streaming path end to end.
#[tokio::test]
async fn thousands_of_tool_updates_are_exactly_once_in_one_prompt_run() {
    const UPDATES_PER_CALL: usize = 1500;
    const CALLS: usize = 3;
    const TOTAL: usize = UPDATES_PER_CALL * CALLS;

    let cwd = TempDir::new("sdk-dedupe-stress");
    let sessions = cwd.path().join("sessions");
    let registry = registry(Arc::new(|index, _context| match index {
        0 => assistant(
            (0..CALLS)
                .map(|call| tool_call_block(&format!("flood-{call}"), "flood_probe", vec![]))
                .collect(),
            StopReason::ToolUse,
        ),
        _ => assistant(vec![text_block("done")], StopReason::Stop),
    }));
    let agent = agent_builder(registry, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Dir(sessions))
        .with_tool(Arc::new(FloodProbeTool {
            updates: UPDATES_PER_CALL,
        }))
        .build()
        .await
        .expect("build flood-probe agent");

    let started = std::time::Instant::now();
    let events: Vec<SdkEvent> = agent
        .prompt_stream("flood the update channel")
        .collect()
        .await;
    let elapsed = started.elapsed();

    let mut counts: std::collections::HashMap<(String, i64), usize> =
        std::collections::HashMap::new();
    let mut per_call_last: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for event in &events {
        if let SdkEvent::ToolExecutionUpdate {
            tool_call_id,
            partial,
            ..
        } = event
        {
            let seq = partial
                .get("seq")
                .and_then(serde_json::Value::as_i64)
                .expect("flood partial carries its sequence number");
            *counts.entry((tool_call_id.clone(), seq)).or_default() += 1;
            let last = per_call_last.entry(tool_call_id.as_str()).or_insert(-1);
            assert!(
                seq > *last,
                "updates for {tool_call_id} must stay in source order: {seq} after {last}"
            );
            *last = seq;
        }
    }

    let duplicates: Vec<_> = counts.iter().filter(|(_, count)| **count != 1).collect();
    assert!(
        duplicates.is_empty(),
        "every (call id, seq) must appear exactly once; duplicates: {duplicates:?}"
    );
    assert_eq!(
        counts.len(),
        TOTAL,
        "all {TOTAL} loop updates must reach the stream (got {})",
        counts.len()
    );
    for call in 0..CALLS {
        let id = format!("flood-{call}");
        assert_eq!(
            per_call_last.get(id.as_str()),
            Some(&((UPDATES_PER_CALL as i64) - 1)),
            "call {id} delivered its full update sequence"
        );
    }
    println!(
        "dedupe stress: {TOTAL} updates across {CALLS} calls in one run, exactly once, {:.3}s",
        elapsed.as_secs_f64()
    );

    agent.shutdown().await.expect("shutdown flood-probe agent");
}

// ---------------------------------------------------------------------------
// Round 21 — long-running real bash streaming: PIR-SDK-008 memory curve
// ---------------------------------------------------------------------------

/// RSS in KiB for `pid`, via the Windows `tasklist` CSV output.
fn tasklist_rss_kib(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|line| line.contains(&pid.to_string()))?;
    let memory = line.rsplit("\",\"").next()?.trim_matches('"');
    let digits: String = memory.chars().filter(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Manual 150 s curve: a REAL bash tool call streams ~250 KiB/s through the
/// SDK. The consumer hashes each partial and drops it (no test-side retention),
/// so RSS reflects only agent/dedupe state. With `ToolUpdateDedupe` the retained
/// fingerprints cover a 15 s window (~7.5 MiB worst case); the old unbounded
/// set would retain ~1500 x ~50 KiB (~75 MiB). Ignored by default because it
/// runs for 2.5 minutes.
#[tokio::test]
#[ignore = "manual 150s real-bash memory curve; run with --ignored --nocapture"]
async fn long_running_real_bash_stream_memory_curve_stays_bounded() {
    use std::collections::HashSet;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    let cwd = TempDir::new("sdk-memory-curve");
    let sessions = cwd.path().join("sessions");
    let python = std::env::var("PIR_MEMORY_CURVE_PYTHON").unwrap_or_else(|_| "python".to_owned());
    let command = format!(
        "'{python}' -c 'import sys,time\nline=\"y\"*5000\nend=time.time()+150\nwhile time.time()<end:\n    sys.stdout.write(line);sys.stdout.write(chr(10));sys.stdout.flush();time.sleep(0.02)'"
    );
    let registry = registry(Arc::new(move |index, _context| match index {
        0 => assistant(
            vec![tool_call_block(
                "bash-memory-curve",
                "bash",
                vec![("command", json!(command)), ("timeout", json!(240))],
            )],
            StopReason::ToolUse,
        ),
        _ => assistant(vec![text_block("curve done")], StopReason::Stop),
    }));
    let agent = agent_builder(registry, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Dir(sessions))
        .core_tools(true)
        .build()
        .await
        .expect("build memory-curve agent");

    let pid = std::process::id();
    let samples: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let samples = Arc::clone(&samples);
        let stop = Arc::clone(&stop);
        let started = Instant::now();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                if let Some(kib) = tasklist_rss_kib(pid) {
                    samples
                        .lock()
                        .unwrap()
                        .push((started.elapsed().as_secs(), kib));
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        })
    };

    let started = Instant::now();
    let mut stream = Box::pin(agent.prompt_stream("stream a long command"));
    let mut updates = 0usize;
    let mut unique: HashSet<u64> = HashSet::new();
    while let Some(event) = stream.as_mut().next().await {
        if let SdkEvent::ToolExecutionUpdate { partial, .. } = &event {
            updates += 1;
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            partial.to_string().hash(&mut hasher);
            unique.insert(hasher.finish());
        }
        if let SdkEvent::ToolExecutionEnd {
            is_error, result, ..
        } = &event
        {
            println!("tool end is_error={is_error} result={result}");
        }
    }
    let elapsed = started.elapsed();
    stop.store(true, Ordering::SeqCst);
    sampler.join().expect("sampler thread");
    agent.shutdown().await.expect("shutdown");

    let samples = samples.lock().unwrap().clone();
    println!(
        "pid={pid} elapsed={elapsed:?} updates={updates} unique={}",
        unique.len()
    );
    for (secs, kib) in &samples {
        println!(
            "rss t={secs:>3}s {kib:>8} KiB ({:.1} MiB)",
            *kib as f64 / 1024.0
        );
    }
    assert_eq!(
        unique.len(),
        updates,
        "each distinct rolling snapshot must be delivered exactly once"
    );
    assert!(
        updates > 300,
        "expected a long update stream, got {updates}"
    );
    assert!(samples.len() >= 8, "not enough RSS samples: {samples:?}");

    let warmup = samples
        .iter()
        .position(|(secs, _)| *secs >= 30)
        .unwrap_or(samples.len() / 4);
    let baseline = samples[..=warmup]
        .iter()
        .map(|(_, kib)| *kib)
        .max()
        .unwrap_or(0);
    let peak_after = samples[warmup..]
        .iter()
        .map(|(_, kib)| *kib)
        .max()
        .unwrap_or(0);
    let growth = peak_after.saturating_sub(baseline);
    println!("baseline={baseline} KiB peak_after_warmup={peak_after} KiB growth={growth} KiB");
    assert!(
        growth < 40 * 1024,
        "dedupe state must stay bounded: RSS grew {growth} KiB after warmup (old unbounded set retained ~75 MiB)"
    );
}

// ---------------------------------------------------------------------------
// Round 21 — three agents streaming concurrently; slow consumers + abort
// ---------------------------------------------------------------------------

async fn drain_stream_slowly(
    agent: &pir::sdk::Agent,
    prompt: &str,
    delay_ms: u64,
    abort_after_first_update: bool,
) -> Vec<SdkEvent> {
    let mut stream = Box::pin(agent.prompt_stream_with_outcome(prompt));
    let mut events = Vec::new();
    let mut updates = 0usize;
    while let Some(event) = stream.as_mut().next().await {
        if matches!(event, SdkEvent::ToolExecutionUpdate { .. }) {
            updates += 1;
            if abort_after_first_update && updates == 1 {
                agent.abort();
            }
        }
        events.push(event);
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    }
    events
}

#[tokio::test]
async fn three_agents_stream_concurrently_with_isolated_events_and_abort() {
    let cwd = TempDir::new("sdk-multi-stream");
    let make_registry = |tag: &'static str, input: u64, output: u64| {
        registry(Arc::new(move |index, _context| {
            match index {
            0 => assistant(
                vec![tool_call_block(
                    &format!("{tag}-bash"),
                    "bash",
                    vec![
                        (
                            "command",
                            json!(format!(
                                "printf '{tag}-1\n'; sleep 0.35; printf '{tag}-2\n'; sleep 0.35; printf '{tag}-3\n'"
                            )),
                        ),
                        ("timeout", json!(30)),
                    ],
                )],
                StopReason::ToolUse,
            ),
            _ => {
                let mut message =
                    assistant(vec![text_block(&format!("{tag} finished"))], StopReason::Stop);
                message.usage = Usage {
                    input,
                    output,
                    total_tokens: input + output,
                    ..Usage::default()
                };
                message
            }
        }
        }))
    };

    let agent_a = agent_builder(make_registry("alpha", 11, 3), cwd.path())
        .agent_dir(cwd.path().join("agent-a"))
        .session(SessionMode::Dir(cwd.path().join("sessions-a")))
        .core_tools(true)
        .build()
        .await
        .expect("build agent a");
    let agent_b = agent_builder(make_registry("beta", 77, 5), cwd.path())
        .agent_dir(cwd.path().join("agent-b"))
        .session(SessionMode::Dir(cwd.path().join("sessions-b")))
        .core_tools(true)
        .build()
        .await
        .expect("build agent b");
    let agent_c = agent_builder(make_registry("gamma", 33, 7), cwd.path())
        .agent_dir(cwd.path().join("agent-c"))
        .session(SessionMode::Dir(cwd.path().join("sessions-c")))
        .core_tools(true)
        .build()
        .await
        .expect("build agent c");

    let started = std::time::Instant::now();
    let (events_a, events_b, events_c) = tokio::join!(
        drain_stream_slowly(&agent_a, "stream alpha", 10, false),
        drain_stream_slowly(&agent_b, "stream beta", 25, false),
        drain_stream_slowly(&agent_c, "stream gamma", 5, true),
    );
    let elapsed = started.elapsed();

    let debug_a = format!("{events_a:?}");
    let debug_b = format!("{events_b:?}");
    let debug_c = format!("{events_c:?}");
    for (tag, debug) in [("alpha", &debug_a), ("beta", &debug_b), ("gamma", &debug_c)] {
        for other in ["alpha", "beta", "gamma"] {
            if other != tag {
                assert!(
                    !debug.contains(&format!("{other}-")),
                    "{tag}'s stream leaked {other}: {debug}"
                );
            }
        }
    }
    for marker in ["alpha-1", "alpha-2", "alpha-3", "alpha finished"] {
        assert!(
            debug_a.contains(marker),
            "alpha missing {marker}: {debug_a}"
        );
    }
    for marker in ["beta-1", "beta-2", "beta-3", "beta finished"] {
        assert!(debug_b.contains(marker), "beta missing {marker}: {debug_b}");
    }
    assert!(
        events_c
            .iter()
            .any(|event| matches!(event, SdkEvent::AgentSettled { aborted: true })),
        "agent c must settle as aborted: {debug_c}"
    );
    assert!(
        events_c.iter().any(|event| matches!(
            event,
            SdkEvent::AgentOutcome(outcome) if outcome.aborted
        )),
        "agent c's outcome must report aborted: {debug_c}"
    );
    assert_eq!(agent_a.usage_totals().total_tokens, 14);
    assert_eq!(agent_b.usage_totals().total_tokens, 82);
    assert_eq!(agent_a.usage_totals().turns, 2);
    assert_eq!(agent_b.usage_totals().turns, 2);
    println!("three concurrent streams completed in {elapsed:?}");

    let path_a = agent_a.session_path().expect("session a");
    let path_b = agent_b.session_path().expect("session b");
    assert_ne!(path_a, path_b, "agents must not share a session file");
    assert!(path_a.starts_with(cwd.path().join("sessions-a")));
    assert!(path_b.starts_with(cwd.path().join("sessions-b")));

    agent_a.shutdown().await.expect("shutdown a");
    agent_b.shutdown().await.expect("shutdown b");
    agent_c.shutdown().await.expect("shutdown c");
}

// ---------------------------------------------------------------------------
// Round 22 — distinct models per agent, shutdown mid tool batch, shared session
// ---------------------------------------------------------------------------

struct NamedScriptedProvider {
    provider_id: String,
    model_id: String,
    calls: AtomicUsize,
    respond: Responder,
    seen_models: Arc<Mutex<Vec<String>>>,
}

fn named_model(provider_id: &str, model_id: &str) -> Model {
    let mut model = mock_model();
    model.id = model_id.to_owned();
    model.name = format!("Scripted {model_id}");
    model.provider = provider_id.to_owned();
    model
}

impl Provider for NamedScriptedProvider {
    fn id(&self) -> &str {
        &self.provider_id
    }

    fn name(&self) -> &str {
        "Named scripted provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![named_model(&self.provider_id, &self.model_id)]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.seen_models.lock().unwrap().push(model.id.clone());
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        if let Some(ContentBlock::Text(text)) = message
            .content
            .iter()
            .find(|block| matches!(block, ContentBlock::Text(_)))
        {
            stream.push(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: text.text.clone(),
                partial: Arc::new(message.clone()),
            });
        }
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn named_provider(
    provider_id: &'static str,
    model_id: &'static str,
    input: u64,
    output: u64,
    marker: &'static str,
    seen: Arc<Mutex<Vec<String>>>,
) -> NamedScriptedProvider {
    NamedScriptedProvider {
        provider_id: provider_id.to_owned(),
        model_id: model_id.to_owned(),
        calls: AtomicUsize::new(0),
        respond: Arc::new(move |_index, _context| {
            let mut message = assistant(vec![text_block(marker)], StopReason::Stop);
            message.provider = provider_id.to_owned();
            message.model = model_id.to_owned();
            message.usage = Usage {
                input,
                output,
                total_tokens: input + output,
                ..Usage::default()
            };
            message
        }),
        seen_models: seen,
    }
}

fn outcome_text(events: &[SdkEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        SdkEvent::AgentOutcome(outcome) => outcome.message_text(),
        _ => None,
    })
}

#[tokio::test]
async fn distinct_models_keep_events_usage_and_model_switches_isolated_across_agents() {
    let cwd = TempDir::new("sdk-multi-model");
    let build = |name: &str, registry: Arc<ProviderRegistry>, model: Model| {
        AgentBuilder::new()
            .with_registry(registry, model)
            .cwd(cwd.path())
            .agent_dir(cwd.path().join(format!("agent-{name}")))
            .session(SessionMode::Dir(
                cwd.path().join(format!("sessions-{name}")),
            ))
            .no_compaction()
            .thinking_level(ThinkingLevel::Off)
    };

    let seen_a = Arc::new(Mutex::new(Vec::new()));
    let seen_b = Arc::new(Mutex::new(Vec::new()));
    let seen_c = Arc::new(Mutex::new(Vec::new()));

    let mut reg_a = ProviderRegistry::default();
    reg_a.set_provider(Arc::new(named_provider(
        "scripted-alpha",
        "alpha-model",
        10,
        1,
        "alpha reply",
        Arc::clone(&seen_a),
    )));
    let mut reg_b = ProviderRegistry::default();
    reg_b.set_provider(Arc::new(named_provider(
        "scripted-beta",
        "beta-model",
        20,
        2,
        "beta reply",
        Arc::clone(&seen_b),
    )));
    let mut reg_c = ProviderRegistry::default();
    reg_c.set_provider(Arc::new(named_provider(
        "scripted-gamma",
        "gamma-model",
        30,
        3,
        "gamma reply",
        Arc::clone(&seen_c),
    )));
    reg_c.set_provider(Arc::new(named_provider(
        "scripted-delta",
        "delta-model",
        40,
        4,
        "delta reply",
        Arc::clone(&seen_c),
    )));

    let agent_a = build(
        "a",
        Arc::new(reg_a),
        named_model("scripted-alpha", "alpha-model"),
    )
    .build()
    .await
    .expect("build agent a");
    let agent_b = build(
        "b",
        Arc::new(reg_b),
        named_model("scripted-beta", "beta-model"),
    )
    .build()
    .await
    .expect("build agent b");
    let agent_c = build(
        "c",
        Arc::new(reg_c),
        named_model("scripted-gamma", "gamma-model"),
    )
    .build()
    .await
    .expect("build agent c");

    let (events_a, events_b, events_c) = tokio::join!(
        agent_a
            .prompt_stream_with_outcome("ask alpha")
            .collect::<Vec<_>>(),
        agent_b
            .prompt_stream_with_outcome("ask beta")
            .collect::<Vec<_>>(),
        agent_c
            .prompt_stream_with_outcome("ask gamma")
            .collect::<Vec<_>>(),
    );

    let debug_a = format!("{events_a:?}");
    let debug_b = format!("{events_b:?}");
    let debug_c = format!("{events_c:?}");
    for (tag, debug) in [("alpha", &debug_a), ("beta", &debug_b), ("gamma", &debug_c)] {
        for other in ["alpha reply", "beta reply", "gamma reply", "delta reply"] {
            if !other.starts_with(tag) {
                assert!(
                    !debug.contains(other),
                    "{tag}'s stream leaked `{other}`: {debug}"
                );
            }
        }
    }
    assert_eq!(outcome_text(&events_a).as_deref(), Some("alpha reply"));
    assert_eq!(outcome_text(&events_b).as_deref(), Some("beta reply"));
    assert_eq!(outcome_text(&events_c).as_deref(), Some("gamma reply"));

    assert_eq!(agent_a.model().provider, "scripted-alpha");
    assert_eq!(agent_a.model().id, "alpha-model");
    assert_eq!(agent_b.model().provider, "scripted-beta");
    assert_eq!(agent_b.model().id, "beta-model");
    assert_eq!(agent_c.model().id, "gamma-model");

    assert_eq!(agent_a.usage_totals().total_tokens, 11);
    assert_eq!(agent_b.usage_totals().total_tokens, 22);
    assert_eq!(agent_c.usage_totals().total_tokens, 33);
    assert_eq!(agent_a.usage_totals().turns, 1);
    assert_eq!(agent_b.usage_totals().turns, 1);
    assert_eq!(agent_c.usage_totals().turns, 1);

    // Switch only agent C to a second model; A/B and C's completed usage stay
    // exactly as they were.
    agent_c
        .set_model(named_model("scripted-delta", "delta-model"))
        .await
        .expect("switch agent c model");
    assert_eq!(agent_c.model().provider, "scripted-delta");
    assert_eq!(agent_c.model().id, "delta-model");
    let delta = agent_c
        .prompt("ask delta")
        .await
        .expect("prompt on delta model");
    assert_eq!(delta.message_text().as_deref(), Some("delta reply"));
    assert_eq!(agent_c.usage_totals().total_tokens, 77);
    assert_eq!(agent_c.usage_totals().turns, 2);
    assert_eq!(agent_a.usage_totals().total_tokens, 11);
    assert_eq!(agent_b.usage_totals().total_tokens, 22);

    // Every provider only ever saw its own model id.
    assert_eq!(seen_a.lock().unwrap().as_slice(), ["alpha-model"]);
    assert_eq!(seen_b.lock().unwrap().as_slice(), ["beta-model"]);
    assert_eq!(
        seen_c.lock().unwrap().as_slice(),
        ["gamma-model", "delta-model"]
    );

    let path_a = agent_a.session_path().expect("session a");
    let path_b = agent_b.session_path().expect("session b");
    let path_c = agent_c.session_path().expect("session c");
    assert_ne!(path_a, path_b);
    assert_ne!(path_b, path_c);

    agent_a.shutdown().await.expect("shutdown a");
    agent_b.shutdown().await.expect("shutdown b");
    agent_c.shutdown().await.expect("shutdown c");
}

/// All lines of a session file parse as JSON and the file ends with a newline:
/// the append-only JSONL invariant.
fn assert_valid_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let bytes = std::fs::read(path).expect("read session file");
    assert!(!bytes.is_empty(), "session file must not be empty");
    assert_eq!(
        bytes.last(),
        Some(&b'\n'),
        "session file must end with a newline"
    );
    let text = String::from_utf8(bytes).expect("session file is UTF-8");
    text.lines()
        .map(|line| {
            assert!(
                !line.trim().is_empty(),
                "session file has a blank JSONL line"
            );
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("invalid JSONL line ({error}): {line}");
            })
        })
        .collect()
}

#[tokio::test]
async fn shutdown_mid_tool_batch_cancels_only_that_agent_and_keeps_its_jsonl_valid() {
    let temp = TempDir::new("sdk-shutdown-batch");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));

    let registry_x = registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call("gate-1", "gate"), tool_call("gate-2", "gate")],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text_block("unexpected completion")], StopReason::Stop)
        }
    }));
    let agent_x = agent_builder(registry_x, temp.path())
        .agent_dir(temp.path().join("agent-x"))
        .session(SessionMode::Dir(temp.path().join("sessions-x")))
        .with_tool(Arc::new(GateTool {
            gate: gate_rx,
            entered: Arc::clone(&entered),
        }))
        .build()
        .await
        .expect("build agent x");

    // A second, independent agent streams a real tool while X is torn down.
    let registry_y = registry(Arc::new(|index, _context| match index {
        0 => assistant(
            vec![tool_call_block(
                "y-bash",
                "bash",
                vec![
                    (
                        "command",
                        json!(
                            "printf 'y-1\n'; sleep 0.3; printf 'y-2\n'; sleep 0.3; printf 'y-3\n'"
                        ),
                    ),
                    ("timeout", json!(30)),
                ],
            )],
            StopReason::ToolUse,
        ),
        _ => {
            let mut message = assistant(vec![text_block("y finished")], StopReason::Stop);
            message.usage = Usage {
                input: 5,
                output: 6,
                total_tokens: 11,
                ..Usage::default()
            };
            message
        }
    }));
    let agent_y = agent_builder(registry_y, temp.path())
        .agent_dir(temp.path().join("agent-y"))
        .session(SessionMode::Dir(temp.path().join("sessions-y")))
        .core_tools(true)
        .build()
        .await
        .expect("build agent y");

    let y_task = tokio::spawn({
        let agent = agent_y.clone();
        async move { drain_stream_slowly(&agent, "stream y", 5, false).await }
    });
    let x_task = tokio::spawn({
        let agent = agent_x.clone();
        async move {
            agent
                .prompt_stream_with_outcome("shutdown batch")
                .collect::<Vec<_>>()
                .await
        }
    });

    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        entered.load(Ordering::SeqCst),
        2,
        "both batch members must be running before shutdown"
    );

    agent_x.shutdown().await.expect("shutdown x");
    let events_x = x_task.await.expect("join x");
    let debug_x = format!("{events_x:?}");
    assert!(
        events_x
            .iter()
            .any(|event| matches!(event, SdkEvent::AgentSettled { aborted: true })),
        "the shut-down run settles as aborted: {debug_x}"
    );
    assert!(
        events_x
            .iter()
            .any(|event| matches!(event, SdkEvent::AgentOutcome(outcome) if outcome.aborted)),
        "the shut-down outcome reports aborted: {debug_x}"
    );
    assert!(
        !debug_x.contains("unexpected completion"),
        "the run never reached a second turn: {debug_x}"
    );

    // The batch tools observed the abort; no new call entered after shutdown.
    let entered_after = entered.load(Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        entered.load(Ordering::SeqCst),
        entered_after,
        "aborted tools are not respawned after shutdown"
    );

    let x_path = agent_x.session_path().expect("session x");
    let x_entries = assert_valid_jsonl(&x_path);
    let x_text = format!("{x_entries:?}");
    assert!(
        x_text.contains("shutdown batch"),
        "the user message reached the session file before shutdown"
    );

    let y_events = y_task.await.expect("join y");
    let debug_y = format!("{y_events:?}");
    for marker in ["y-1", "y-2", "y-3", "y finished"] {
        assert!(
            debug_y.contains(marker),
            "surviving agent missing {marker}: {debug_y}"
        );
    }
    assert!(y_events
        .iter()
        .any(|event| matches!(event, SdkEvent::AgentSettled { aborted: false })));
    assert_eq!(agent_y.usage_totals().total_tokens, 11);
    let y_path = agent_y.session_path().expect("session y");
    assert_ne!(x_path, y_path, "agents keep distinct session files");
    let y_entries = assert_valid_jsonl(&y_path);
    assert!(
        format!("{y_entries:?}").contains("y finished"),
        "the surviving agent's turn is persisted"
    );

    let error = agent_x
        .prompt("post shutdown")
        .await
        .expect_err("prompts after shutdown are rejected");
    assert!(
        error.to_string().to_lowercase().contains("shut down"),
        "the rejection names shutdown: {error}"
    );
    gate_tx.send(true).expect("release gate");
    agent_y.shutdown().await.expect("shutdown y");
}

#[tokio::test]
async fn two_agents_attached_to_one_session_file_append_valid_but_divergent_branches() {
    let temp = TempDir::new("sdk-shared-session");
    let seed_dir = temp.path().join("seed");
    let seed_registry = registry(Arc::new(|_index, _context| {
        assistant(vec![text_block("seed reply")], StopReason::Stop)
    }));
    let seed = agent_builder(seed_registry, temp.path())
        .agent_dir(seed_dir.join("agent"))
        .session(SessionMode::Dir(seed_dir.join("sessions")))
        .build()
        .await
        .expect("build seed agent");
    seed.prompt("seed question").await.expect("seed prompt");
    let path = seed.session_path().expect("seed session path");
    let seed_id = seed.session_id().expect("seed session id");
    seed.shutdown().await.expect("seed shutdown");
    drop(seed);

    let build = |name: &'static str, reply: &'static str| {
        let registry = registry(Arc::new(move |_index, _context| {
            assistant(vec![text_block(reply)], StopReason::Stop)
        }));
        let dir = temp.path().join(name);
        agent_builder(registry, temp.path())
            .agent_dir(dir.join("agent"))
            .session(SessionMode::Dir(dir.join("sessions")))
            .build()
    };
    let agent_a = build("writer-a", "reply a").await.expect("build writer a");
    let agent_b = build("writer-b", "reply b").await.expect("build writer b");
    agent_a.attach_session(&path).await.expect("attach a");
    agent_b.attach_session(&path).await.expect("attach b");
    assert_eq!(agent_a.session_id(), Some(seed_id.clone()));
    assert_eq!(agent_b.session_id(), Some(seed_id.clone()));

    let (outcome_a, outcome_b) = tokio::join!(agent_a.prompt("from a"), agent_b.prompt("from b"),);
    assert_eq!(
        outcome_a.expect("prompt a").message_text().as_deref(),
        Some("reply a")
    );
    assert_eq!(
        outcome_b.expect("prompt b").message_text().as_deref(),
        Some("reply b")
    );

    // Both writers appended complete JSONL rows: the file itself stays a valid
    // append-only log even though the two managers never coordinate.
    let entries = assert_valid_jsonl(&path);
    let raw = format!("{entries:?}");
    assert!(
        raw.contains("from a"),
        "writer a's user row is on disk: {raw}"
    );
    assert!(
        raw.contains("from b"),
        "writer b's user row is on disk: {raw}"
    );
    assert!(raw.contains("reply a"), "writer a's reply row is on disk");
    assert!(raw.contains("reply b"), "writer b's reply row is on disk");

    // A fresh reader sees ONE branch of the append tree (the active leaf);
    // the other writer's rows remain in the file as an unselected sibling
    // branch. This is the upstream append-tree semantic, not byte corruption.
    let reader_registry = registry(Arc::new(|_index, _context| {
        assistant(vec![text_block("reader reply")], StopReason::Stop)
    }));
    let reader = agent_builder(reader_registry, temp.path())
        .agent_dir(temp.path().join("reader-agent"))
        .session(SessionMode::Dir(temp.path().join("reader-sessions")))
        .build()
        .await
        .expect("build reader");
    reader.attach_session(&path).await.expect("attach reader");
    let transcript = reader.transcript().await;
    let texts: Vec<String> = transcript.iter().map(message_text).collect();
    let sees_a = texts.iter().any(|text| text.contains("from a"));
    let sees_b = texts.iter().any(|text| text.contains("from b"));
    println!(
        "shared-session reopen: {} entries on disk, transcript sees a={sees_a} b={sees_b}: {texts:?}",
        entries.len()
    );
    assert!(
        sees_a || sees_b,
        "the reopened transcript contains at least the active branch: {texts:?}"
    );

    reader.shutdown().await.expect("shutdown reader");
    agent_a.shutdown().await.expect("shutdown a");
    agent_b.shutdown().await.expect("shutdown b");
}

// ---------------------------------------------------------------------------
// Round 22 — dispatch_command (builtin todo/memory + inline) vs tool path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dispatch_command_composes_builtin_todo_memory_and_inline_commands_with_tool_path() {
    let temp = TempDir::new("sdk-dispatch-combo");
    let events = Arc::new(Mutex::new(Vec::new()));
    let command_args = Arc::new(Mutex::new(Vec::new()));
    let registry = registry(Arc::new(|index, _context| match index {
        0 => assistant(
            vec![
                tool_call_block(
                    "mem-recall",
                    "memory",
                    vec![
                        ("action", json!("recall")),
                        ("query", json!("dispatch-seeded")),
                    ],
                ),
                tool_call_block(
                    "todo-create",
                    "todo",
                    vec![
                        ("action", json!("create")),
                        ("subject", json!("dispatch task")),
                    ],
                ),
            ],
            StopReason::ToolUse,
        ),
        _ => assistant(vec![text_block("combo done")], StopReason::Stop),
    }));
    let agent = agent_builder(registry, temp.path())
        .agent_dir(temp.path().join("agent"))
        .session(SessionMode::None)
        .with_plugin(Arc::new(AuditPlugin {
            info: PluginInfo::inline("audit-plugin"),
            events: Arc::clone(&events),
            command_args: Arc::clone(&command_args),
        }))
        .builtin("todo")
        .builtin("memory")
        .build()
        .await
        .expect("build dispatch agent");

    let commands = agent.commands();
    for name in ["todos", "memory", "audit"] {
        assert!(
            commands.iter().any(|(candidate, _)| candidate == name),
            "command `{name}` is registered: {commands:?}"
        );
    }

    let todos = agent
        .dispatch_command("todos", "")
        .await
        .expect("dispatch todos");
    // Headless SDK sessions have no interactive UI; the builtin reports its
    // gate instead of touching the store.
    assert!(!todos.ok, "todos must be gated headless: {todos:?}");
    assert!(todos
        .error
        .as_deref()
        .is_some_and(|text| text.to_lowercase().contains("interactive")));

    let init = agent
        .dispatch_command("memory", "init")
        .await
        .expect("dispatch memory init");
    assert!(init.ok, "memory init succeeds: {init:?}");
    let remember = agent
        .dispatch_command(
            "memory",
            "remember --kind fact --subject dispatch-seeded --text dispatchnote",
        )
        .await
        .expect("dispatch memory remember");
    assert!(remember.ok, "memory remember succeeds: {remember:?}");
    let recall = agent
        .dispatch_command("memory", "recall --query dispatch-seeded")
        .await
        .expect("dispatch memory recall");
    assert!(recall.ok, "memory recall succeeds: {recall:?}");
    assert!(
        recall
            .error
            .as_deref()
            .is_some_and(|text| text.contains("dispatchnote")),
        "the command sees the record written by the previous command: {recall:?}"
    );

    let audit = agent
        .dispatch_command("audit", "hello")
        .await
        .expect("dispatch inline command");
    assert_eq!(
        audit.error.as_deref(),
        Some("plugin command ran with args=hello")
    );
    assert_eq!(command_args.lock().unwrap().as_slice(), ["audit:hello"]);

    // The same memory store is visible through the TOOL path: the model asks
    // the `memory` tool for the record inserted by dispatch_command.
    let outcome = agent
        .prompt("recall through the tool")
        .await
        .expect("prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("combo done"));
    let tool_text = outcome.tool_result_texts().join("\n");
    assert!(
        tool_text.contains("dispatchnote"),
        "the memory tool reads the dispatch-written record: {tool_text}"
    );
    assert!(
        tool_text.contains("dispatch task"),
        "the todo builtin tool still works beside the commands: {tool_text}"
    );

    // dispatch_command does not start a prompt run; only the prompt emits the
    // input/before_agent_start lifecycle pair once.
    let log = events.lock().unwrap().clone();
    assert_eq!(
        log.iter().filter(|name| name.as_str() == "input").count(),
        1,
        "one input event for the single prompt: {log:?}"
    );
    assert_eq!(
        log.iter()
            .filter(|name| name.as_str() == "before_agent_start")
            .count(),
        1,
        "one before_agent_start for the single prompt: {log:?}"
    );

    agent.shutdown().await.expect("shutdown dispatch agent");
}

// ---------------------------------------------------------------------------
// Round 23 — mid-stream model switch, shutdown with foreground+background
// tools, completed-background shutdown idempotency, command/config boundaries
// ---------------------------------------------------------------------------

use pir::plugins::builtin::MemoryPlugin;

/// A provider whose first stream pauses after its first delta until the test
/// releases it. The first turn asks for `fast_tool`; the second turn answers
/// with text naming the model actually used for that request.
struct PausedModelProvider {
    calls: AtomicUsize,
    seen_models: Arc<Mutex<Vec<String>>>,
    /// `(model id, context_window, max_tokens)` per request: a switch must
    /// carry the NEW model's limits into the request, not the run-start ones.
    seen_limits: Arc<Mutex<Vec<(String, u64, u64)>>>,
    release: tokio::sync::watch::Receiver<bool>,
}

impl Provider for PausedModelProvider {
    fn id(&self) -> &str {
        "scripted-paused"
    }

    fn name(&self) -> &str {
        "Paused scripted provider"
    }

    fn get_models(&self) -> Vec<Model> {
        let mut delta = named_model("scripted-paused", "delta-model");
        // Distinct limits: the next request must use the NEW model's values.
        delta.context_window = 65_536;
        delta.max_tokens = 4_096;
        vec![named_model("scripted-paused", "alpha-model"), delta]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.seen_models.lock().unwrap().push(model.id.clone());
        self.seen_limits.lock().unwrap().push((
            model.id.clone(),
            model.context_window,
            model.max_tokens,
        ));
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        let producer = stream.clone();
        let mut release = self.release.clone();
        let model_id = model.id.clone();
        let provider_id = model.provider.clone();
        tokio::spawn(async move {
            if index == 0 {
                let mut partial = assistant(vec![text_block("working")], StopReason::Stop);
                partial.provider = provider_id.clone();
                partial.model = model_id.clone();
                producer.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "working".to_owned(),
                    partial: Arc::new(partial),
                });
                let _ = release.changed().await;
                let mut message = assistant(
                    vec![tool_call("paused-tool", "fast_tool")],
                    StopReason::ToolUse,
                );
                message.provider = provider_id;
                message.model = model_id;
                message.usage = Usage {
                    input: 10,
                    output: 1,
                    total_tokens: 11,
                    ..Usage::default()
                };
                producer.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    message,
                ));
            } else {
                let text = if model_id == "delta-model" {
                    "delta reply".to_owned()
                } else {
                    format!("{model_id} reply")
                };
                let mut message = assistant(vec![text_block(&text)], StopReason::Stop);
                message.provider = provider_id;
                message.model = model_id;
                message.usage = Usage {
                    input: 40,
                    output: 4,
                    total_tokens: 44,
                    ..Usage::default()
                };
                producer.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: text,
                    partial: Arc::new(message.clone()),
                });
                producer.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    message,
                ));
            }
        });
        stream
    }
}

struct FastTool;

impl AgentTool for FastTool {
    fn name(&self) -> &'static str {
        "fast_tool"
    }

    fn description(&self) -> &'static str {
        "returns immediately"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object"}))
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("fast tool done") })
    }
}

/// Records `model_select` / `session_shutdown` events and counts teardown.
struct LifecyclePlugin {
    info: PluginInfo,
    events: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    shutdowns: Arc<AtomicUsize>,
}

impl AgentPlugin for LifecyclePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(
            ["model_select", "session_shutdown"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        self.events
            .lock()
            .unwrap()
            .push((event.name.to_string(), event.payload.clone()));
        Box::pin(async { PluginEventOutcome::None })
    }

    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        self.events
            .lock()
            .unwrap()
            .push(("on_shutdown".to_owned(), json!({})));
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// A tool that blocks until the run is aborted.
struct HoldTool {
    name: &'static str,
    entered: Arc<AtomicUsize>,
}

impl AgentTool for HoldTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "blocks until aborted"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object"}))
    }

    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let aborted = ctx.signal.clone();
        Box::pin(async move {
            loop {
                if aborted.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    return ToolResult::error("aborted");
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
    }
}

/// Never returns and ignores the abort signal: makes the settle wait hit the
/// SDK's bounded shutdown timeout.
struct WedgedTool {
    entered: Arc<AtomicUsize>,
}

impl AgentTool for WedgedTool {
    fn name(&self) -> &'static str {
        "wedge_fg"
    }

    fn description(&self) -> &'static str {
        "never returns, ignores aborts"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object"}))
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
    }
}

/// Records `session_shutdown`/`on_shutdown` and takes `delay` inside the hook,
/// proving a slow teardown hook still runs exactly once.
struct SlowShutdownPlugin {
    info: PluginInfo,
    events: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    shutdowns: Arc<AtomicUsize>,
    delay: std::time::Duration,
}

impl AgentPlugin for SlowShutdownPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(["session_shutdown".to_owned()].into_iter().collect())
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        self.events
            .lock()
            .unwrap()
            .push((event.name.to_string(), event.payload.clone()));
        Box::pin(async { PluginEventOutcome::None })
    }

    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        self.events
            .lock()
            .unwrap()
            .push(("on_shutdown".to_owned(), serde_json::json!({})));
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(())
        })
    }
}

struct QuickBackgroundTool;

impl AgentTool for QuickBackgroundTool {
    fn name(&self) -> &'static str {
        "quick_bg"
    }

    fn description(&self) -> &'static str {
        "finishes immediately"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object"}))
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("background done") })
    }
}

// PIR-SDK-009 regression: a `set_model` issued while a run is streaming must
// reach the run's NEXT model request. Upstream
// `AgentSession.prepareNextTurnWithContext` returns `model:
// this.agent.state.model` on every turn boundary
// (packages/coding-agent/src/core/agent-session.ts:904-931); the session now
// installs the equivalent `prepare_next_turn` hook so the in-flight request
// keeps its old model while the next turn re-reads the live cell.
#[tokio::test]
async fn set_model_mid_stream_switches_next_request_and_emits_model_select() {
    let temp = TempDir::new("sdk-set-model-midstream");
    let seen_a = Arc::new(Mutex::new(Vec::new()));
    let seen_limits_a: Arc<Mutex<Vec<(String, u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let mut reg_a = ProviderRegistry::default();
    reg_a.set_provider(Arc::new(PausedModelProvider {
        calls: AtomicUsize::new(0),
        seen_models: Arc::clone(&seen_a),
        seen_limits: Arc::clone(&seen_limits_a),
        release: release_rx,
    }));
    let plugin_a_events = Arc::new(Mutex::new(Vec::new()));
    let shutdowns_a = Arc::new(AtomicUsize::new(0));
    let agent_a = AgentBuilder::new()
        .with_registry(
            Arc::new(reg_a),
            named_model("scripted-paused", "alpha-model"),
        )
        .cwd(temp.path())
        .agent_dir(temp.path().join("agent-a"))
        .session(SessionMode::Dir(temp.path().join("sessions-a")))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .with_tool(Arc::new(FastTool))
        .with_plugin(Arc::new(LifecyclePlugin {
            info: PluginInfo::inline("lifecycle-a"),
            events: Arc::clone(&plugin_a_events),
            shutdowns: Arc::clone(&shutdowns_a),
        }))
        .build()
        .await
        .expect("build agent a");

    // A second agent streams concurrently; its model/events must stay its own.
    let seen_b = Arc::new(Mutex::new(Vec::new()));
    let mut reg_b = ProviderRegistry::default();
    reg_b.set_provider(Arc::new(named_provider(
        "scripted-beta",
        "beta-model",
        20,
        2,
        "beta reply",
        Arc::clone(&seen_b),
    )));
    let plugin_b_events = Arc::new(Mutex::new(Vec::new()));
    let shutdowns_b = Arc::new(AtomicUsize::new(0));
    let agent_b = AgentBuilder::new()
        .with_registry(Arc::new(reg_b), named_model("scripted-beta", "beta-model"))
        .cwd(temp.path())
        .agent_dir(temp.path().join("agent-b"))
        .session(SessionMode::Dir(temp.path().join("sessions-b")))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .with_plugin(Arc::new(LifecyclePlugin {
            info: PluginInfo::inline("lifecycle-b"),
            events: Arc::clone(&plugin_b_events),
            shutdowns: Arc::clone(&shutdowns_b),
        }))
        .build()
        .await
        .expect("build agent b");

    let b_task = tokio::spawn({
        let agent = agent_b.clone();
        async move {
            agent
                .prompt_stream_with_outcome("ask beta")
                .collect::<Vec<_>>()
                .await
        }
    });

    // Consume agent A's stream on a task and signal as soon as the paused
    // first delta reaches the consumer (the provider is still mid-stream).
    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    let a_task = tokio::spawn({
        let agent = agent_a.clone();
        async move {
            let mut events = Vec::new();
            let stream = agent.prompt_stream_with_outcome("ask alpha");
            futures_util::pin_mut!(stream);
            let mut announced = false;
            let mut first_tx = Some(first_tx);
            while let Some(event) = stream.next().await {
                if !announced {
                    if matches!(&event, SdkEvent::MessageUpdate { delta, .. } if delta == "working")
                    {
                        announced = true;
                        if let Some(tx) = first_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                }
                events.push(event);
            }
            events
        }
    });

    first_rx.await.expect("first delta observed");
    assert_eq!(agent_a.model().id, "alpha-model");
    // The switch carries a model OBJECT with limits different from the
    // run-start model; the next request must use these values.
    let mut delta_model = named_model("scripted-paused", "delta-model");
    delta_model.context_window = 65_536;
    delta_model.max_tokens = 4_096;
    agent_a
        .set_model(delta_model)
        .await
        .expect("switch model mid-stream");
    assert_eq!(agent_a.model().id, "delta-model");
    release_tx.send(true).expect("release the paused stream");

    let stream_a = a_task.await.expect("join agent a's stream");
    let debug_a = format!("{stream_a:?}");
    assert!(
        debug_a.contains("working"),
        "the in-flight stream kept its first turn: {debug_a}"
    );
    assert!(
        stream_a
            .iter()
            .any(|event| matches!(event, SdkEvent::AgentSettled { aborted: false })),
        "the switched run settles normally: {debug_a}"
    );

    // The switch reaches the plugin face exactly once with full model objects.
    let log_a = plugin_a_events.lock().unwrap().clone();
    let selects: Vec<_> = log_a
        .iter()
        .filter(|(name, _)| name == "model_select")
        .collect();
    assert_eq!(selects.len(), 1, "exactly one model_select: {log_a:?}");
    assert_eq!(selects[0].1["model"]["id"], "delta-model");
    assert_eq!(selects[0].1["previousModel"]["id"], "alpha-model");
    assert_eq!(selects[0].1["source"], "set");

    let stream_b = b_task.await.expect("join agent b");
    assert_eq!(outcome_text(&stream_b).as_deref(), Some("beta reply"));
    assert_eq!(seen_b.lock().unwrap().as_slice(), ["beta-model"]);
    assert_eq!(agent_b.model().id, "beta-model");
    assert!(
        plugin_b_events
            .lock()
            .unwrap()
            .iter()
            .all(|(name, _)| name != "model_select"),
        "a concurrent agent never sees another agent's model switch"
    );

    // PIR-SDK-009: these two assertions encode the upstream per-turn refresh
    // (`prepareNextTurnWithContext` returns the live `agent.state.model`) and
    // currently fail — the run stays on alpha for its second request.
    assert_eq!(outcome_text(&stream_a).as_deref(), Some("delta reply"));
    assert_eq!(
        seen_a.lock().unwrap().as_slice(),
        ["alpha-model", "delta-model"],
        "the next request after the switch uses the new model"
    );
    assert_eq!(
        seen_limits_a.lock().unwrap().as_slice(),
        [
            ("alpha-model".to_owned(), 8_192, 512),
            ("delta-model".to_owned(), 65_536, 4_096)
        ],
        "the switched request carries the new model's context_window/max_tokens"
    );
    assert_eq!(agent_a.usage_totals().total_tokens, 55);
    assert_eq!(agent_a.usage_totals().turns, 2);

    // The persisted run attributes each assistant message to the model that
    // actually served it, and the switch is recorded as a branch change.
    let path = agent_a.session_path().expect("session path");
    let entries = assert_valid_jsonl(&path);
    let assistant_models: Vec<String> = entries
        .iter()
        .filter(|entry| entry["type"] == "message")
        .filter(|entry| entry["message"]["role"] == "assistant")
        .map(|entry| {
            entry["message"]["model"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        assistant_models,
        ["alpha-model", "delta-model"],
        "session JSONL records the model that served each turn"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry["type"] == "model_change" && entry["modelId"] == "delta-model"),
        "the mid-run model switch is recorded on the branch"
    );

    agent_a.shutdown().await.expect("shutdown agent a");
    agent_b.shutdown().await.expect("shutdown agent b");
}

/// A reasoning-capable variant of [`mock_model`] (the default does not reason,
/// so every thinking level would clamp to `off`).
fn reasoning_model(provider_id: &str, model_id: &str) -> Model {
    let mut model = named_model(provider_id, model_id);
    model.reasoning = true;
    model
}

/// Pauses after its first delta like [`PausedModelProvider`] and records the
/// thinking level carried by every stream request.
struct PausedThinkingProvider {
    calls: AtomicUsize,
    seen_thinking: Arc<Mutex<Vec<Option<String>>>>,
    release: tokio::sync::watch::Receiver<bool>,
}

impl Provider for PausedThinkingProvider {
    fn id(&self) -> &str {
        "scripted-paused-thinking"
    }

    fn name(&self) -> &str {
        "Paused thinking provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![reasoning_model(
            "scripted-paused-thinking",
            "reasoner-model",
        )]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.seen_thinking
            .lock()
            .unwrap()
            .push(options.reasoning.map(|level| level.wire_key().to_owned()));
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        let producer = stream.clone();
        let mut release = self.release.clone();
        let model_id = model.id.clone();
        let provider_id = model.provider.clone();
        tokio::spawn(async move {
            if index == 0 {
                let mut partial = assistant(vec![text_block("working")], StopReason::Stop);
                partial.provider = provider_id.clone();
                partial.model = model_id.clone();
                producer.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "working".to_owned(),
                    partial: Arc::new(partial),
                });
                let _ = release.changed().await;
                let mut message = assistant(
                    vec![tool_call("paused-tool", "fast_tool")],
                    StopReason::ToolUse,
                );
                message.provider = provider_id;
                message.model = model_id;
                message.usage = Usage {
                    input: 10,
                    output: 1,
                    total_tokens: 11,
                    ..Usage::default()
                };
                producer.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    message,
                ));
            } else {
                let mut message = assistant(vec![text_block("thinking reply")], StopReason::Stop);
                message.provider = provider_id;
                message.model = model_id;
                message.usage = Usage {
                    input: 40,
                    output: 4,
                    total_tokens: 44,
                    ..Usage::default()
                };
                producer.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "thinking reply".to_owned(),
                    partial: Arc::new(message.clone()),
                });
                producer.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    message,
                ));
            }
        });
        stream
    }
}

#[tokio::test]
async fn set_thinking_level_mid_stream_applies_to_next_request() {
    let temp = TempDir::new("sdk-set-thinking-midstream");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let mut reg = ProviderRegistry::default();
    reg.set_provider(Arc::new(PausedThinkingProvider {
        calls: AtomicUsize::new(0),
        seen_thinking: Arc::clone(&seen),
        release: release_rx,
    }));
    let agent = AgentBuilder::new()
        .with_registry(
            Arc::new(reg),
            reasoning_model("scripted-paused-thinking", "reasoner-model"),
        )
        .cwd(temp.path())
        .agent_dir(temp.path().join("agent"))
        .session(SessionMode::Dir(temp.path().join("sessions")))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .with_tool(Arc::new(FastTool))
        .build()
        .await
        .expect("build agent");

    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn({
        let agent = agent.clone();
        async move {
            let mut events = Vec::new();
            let stream = agent.prompt_stream_with_outcome("ask");
            futures_util::pin_mut!(stream);
            let mut announced = false;
            let mut first_tx = Some(first_tx);
            while let Some(event) = stream.next().await {
                if !announced {
                    if matches!(&event, SdkEvent::MessageUpdate { delta, .. } if delta == "working")
                    {
                        announced = true;
                        if let Some(tx) = first_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                }
                events.push(event);
            }
            events
        }
    });

    first_rx.await.expect("first delta observed");
    assert_eq!(agent.thinking_level(), ThinkingLevel::Off);
    let effective = agent
        .set_thinking_level(ThinkingLevel::High)
        .await
        .expect("raise thinking mid-stream");
    assert_eq!(effective, ThinkingLevel::High);
    assert_eq!(agent.thinking_level(), ThinkingLevel::High);
    release_tx.send(true).expect("release the paused stream");

    let events = task.await.expect("join stream");
    let debug = format!("{events:?}");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SdkEvent::AgentSettled { aborted: false })),
        "the run settles normally: {debug}"
    );
    assert_eq!(outcome_text(&events).as_deref(), Some("thinking reply"));
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [Some("off".to_owned()), Some("high".to_owned())],
        "the in-flight request kept its level; the next request picked up the new one"
    );
    assert_eq!(agent.usage_totals().total_tokens, 55);
    assert_eq!(agent.usage_totals().turns, 2);

    agent.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn shutdown_with_foreground_and_background_tools_cancels_both_once() {
    let temp = TempDir::new("sdk-shutdown-mixed");
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![
                    tool_call("fg-call", "hold_fg"),
                    tool_call_block("bg-call", "hold_bg", vec![("background", json!(true))]),
                ],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text_block("unexpected completion")], StopReason::Stop)
        }
    }));
    let plugin_events = Arc::new(Mutex::new(Vec::new()));
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let agent = agent_builder(registry, temp.path())
        .agent_dir(temp.path().join("agent"))
        .session(SessionMode::Dir(temp.path().join("sessions")))
        .with_tool(Arc::new(HoldTool {
            name: "hold_fg",
            entered: Arc::clone(&entered),
        }))
        .with_tool(Arc::new(HoldTool {
            name: "hold_bg",
            entered: Arc::clone(&entered),
        }))
        .with_plugin(Arc::new(LifecyclePlugin {
            info: PluginInfo::inline("lifecycle-shutdown"),
            events: Arc::clone(&plugin_events),
            shutdowns: Arc::clone(&shutdowns),
        }))
        .build()
        .await
        .expect("build agent");

    let background_ends: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&background_ends);
    agent.on_session_event(move |event| {
        if let Some(SdkEvent::BackgroundToolEnd { call_id, .. }) =
            SdkEvent::from_session_event(&event)
        {
            captured.lock().unwrap().push(call_id);
        }
    });

    let stream_task = tokio::spawn({
        let agent = agent.clone();
        async move {
            agent
                .prompt_stream_with_outcome("mixed shutdown")
                .collect::<Vec<_>>()
                .await
        }
    });
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        entered.load(Ordering::SeqCst),
        2,
        "both the foreground and the detached background tool are running"
    );

    agent.shutdown().await.expect("shutdown");
    let events = stream_task.await.expect("join stream");
    let debug = format!("{events:?}");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SdkEvent::AgentSettled { aborted: true })),
        "the run settles aborted: {debug}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SdkEvent::AgentOutcome(outcome) if outcome.aborted)),
        "the outcome reports aborted: {debug}"
    );
    assert!(
        !debug.contains("unexpected completion"),
        "shutdown happened before a second turn: {debug}"
    );
    let entered_after = entered.load(Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        entered.load(Ordering::SeqCst),
        entered_after,
        "cancelled tools are not respawned"
    );
    assert!(
        background_ends.lock().unwrap().is_empty(),
        "a cancelled background call must not emit BackgroundToolEnd: {:?}",
        background_ends.lock().unwrap()
    );

    let path = agent.session_path().expect("session path");
    assert_valid_jsonl(&path);

    let log = plugin_events.lock().unwrap().clone();
    let shutdowns_events: Vec<_> = log
        .iter()
        .filter(|(name, _)| name == "session_shutdown")
        .collect();
    assert_eq!(shutdowns_events.len(), 1, "one session_shutdown: {log:?}");
    assert_eq!(shutdowns_events[0].1["reason"], "quit");
    let hook_pos = log
        .iter()
        .position(|(name, _)| name == "on_shutdown")
        .expect("on_shutdown ran");
    let event_pos = log
        .iter()
        .position(|(name, _)| name == "session_shutdown")
        .expect("session_shutdown ran");
    assert!(
        event_pos < hook_pos,
        "session_shutdown precedes on_shutdown: {log:?}"
    );
    assert_eq!(shutdowns.load(Ordering::SeqCst), 1);

    // Repeated shutdown is idempotent: no second event, hook, or callback.
    agent.shutdown().await.expect("repeat shutdown 1");
    agent.shutdown().await.expect("repeat shutdown 2");
    assert_eq!(
        shutdowns.load(Ordering::SeqCst),
        1,
        "on_shutdown is one-shot"
    );
    let log = plugin_events.lock().unwrap().clone();
    assert_eq!(
        log.iter()
            .filter(|(name, _)| name == "session_shutdown")
            .count(),
        1,
        "session_shutdown is one-shot: {log:?}"
    );
    assert!(
        background_ends.lock().unwrap().is_empty(),
        "no background completion after repeated shutdown"
    );
    let error = agent
        .prompt("post shutdown")
        .await
        .expect_err("prompts after shutdown are rejected");
    assert!(
        error.to_string().to_lowercase().contains("shut down"),
        "the rejection names shutdown: {error}"
    );
}

#[tokio::test]
async fn shutdown_after_background_completion_does_not_repeat_callbacks() {
    let temp = TempDir::new("sdk-shutdown-after-bg");
    let registry = registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call_block(
                    "bg-quick",
                    "quick_bg",
                    vec![("background", json!(true))],
                )],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text_block("bg started")], StopReason::Stop)
        }
    }));
    let plugin_events = Arc::new(Mutex::new(Vec::new()));
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let agent = agent_builder(registry, temp.path())
        .agent_dir(temp.path().join("agent"))
        .session(SessionMode::Dir(temp.path().join("sessions")))
        .with_tool(Arc::new(QuickBackgroundTool))
        .with_plugin(Arc::new(LifecyclePlugin {
            info: PluginInfo::inline("lifecycle-bg"),
            events: Arc::clone(&plugin_events),
            shutdowns: Arc::clone(&shutdowns),
        }))
        .build()
        .await
        .expect("build agent");

    let background_ends: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&background_ends);
    agent.on_session_event(move |event| {
        if let Some(SdkEvent::BackgroundToolEnd { call_id, .. }) =
            SdkEvent::from_session_event(&event)
        {
            captured.lock().unwrap().push(call_id);
        }
    });

    let outcome = agent
        .prompt("start quick background")
        .await
        .expect("prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("bg started"));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while background_ends.lock().unwrap().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the background completion arrived"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(background_ends.lock().unwrap().as_slice(), ["bg-quick"]);

    agent.shutdown().await.expect("shutdown after completion");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        background_ends.lock().unwrap().as_slice(),
        ["bg-quick"],
        "shutdown must not re-deliver a completed background call"
    );
    assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
    let log = plugin_events.lock().unwrap().clone();
    assert_eq!(
        log.iter()
            .filter(|(name, _)| name == "session_shutdown")
            .count(),
        1,
        "one teardown for a completed background session: {log:?}"
    );
    assert_valid_jsonl(&agent.session_path().expect("session path"));
    agent.shutdown().await.expect("repeat shutdown");
    assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_two_backgrounds_one_wedged_foreground_bounds_and_tears_down_once() {
    let temp = TempDir::new("sdk-shutdown-2bg-1fg");
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![
                    tool_call("wedge-call", "wedge_fg"),
                    tool_call_block("bg-a-call", "hold_bg_a", vec![("background", json!(true))]),
                    tool_call_block("bg-b-call", "hold_bg_b", vec![("background", json!(true))]),
                ],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text_block("unexpected completion")], StopReason::Stop)
        }
    }));
    let plugin_events = Arc::new(Mutex::new(Vec::new()));
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let agent = agent_builder(registry, temp.path())
        .agent_dir(temp.path().join("agent"))
        .session(SessionMode::Dir(temp.path().join("sessions")))
        .with_tool(Arc::new(WedgedTool {
            entered: Arc::clone(&entered),
        }))
        .with_tool(Arc::new(HoldTool {
            name: "hold_bg_a",
            entered: Arc::clone(&entered),
        }))
        .with_tool(Arc::new(HoldTool {
            name: "hold_bg_b",
            entered: Arc::clone(&entered),
        }))
        .with_plugin(Arc::new(SlowShutdownPlugin {
            info: PluginInfo::inline("slow-shutdown"),
            events: Arc::clone(&plugin_events),
            shutdowns: Arc::clone(&shutdowns),
            delay: std::time::Duration::from_millis(300),
        }))
        .build()
        .await
        .expect("build agent");

    let background_ends: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&background_ends);
    agent.on_session_event(move |event| {
        if let Some(SdkEvent::BackgroundToolEnd { call_id, .. }) =
            SdkEvent::from_session_event(&event)
        {
            captured.lock().unwrap().push(call_id);
        }
    });

    let stream_task = tokio::spawn({
        let agent = agent.clone();
        async move {
            agent
                .prompt_stream_with_outcome("two backgrounds and a wedge")
                .collect::<Vec<_>>()
                .await
        }
    });
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        entered.load(Ordering::SeqCst),
        3,
        "one foreground and both background tools are running"
    );

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), agent.shutdown())
        .await
        .expect("shutdown is bounded");
    let elapsed = started.elapsed();
    assert!(
        matches!(&result, Err(SdkError::Other(message)) if message.contains("timed out")),
        "the wedged foreground makes shutdown time out: {result:?}"
    );
    assert!(
        elapsed >= std::time::Duration::from_secs(2),
        "the settle wait used its 2s budget: {elapsed:?}"
    );

    let entered_after = entered.load(Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        entered.load(Ordering::SeqCst),
        entered_after,
        "cancelled background tools are not respawned"
    );
    assert!(
        background_ends.lock().unwrap().is_empty(),
        "cancelled background calls emit no BackgroundToolEnd: {:?}",
        background_ends.lock().unwrap()
    );

    let log = plugin_events.lock().unwrap().clone();
    assert_eq!(
        log.iter()
            .filter(|(name, _)| name == "session_shutdown")
            .count(),
        1,
        "one session_shutdown even when the settle timed out: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|(name, _)| name == "on_shutdown").count(),
        1,
        "the slow on_shutdown hook ran exactly once: {log:?}"
    );
    let hook_pos = log
        .iter()
        .position(|(name, _)| name == "on_shutdown")
        .expect("on_shutdown ran");
    let event_pos = log
        .iter()
        .position(|(name, _)| name == "session_shutdown")
        .expect("session_shutdown ran");
    assert!(
        event_pos < hook_pos,
        "session_shutdown precedes on_shutdown"
    );
    assert_eq!(shutdowns.load(Ordering::SeqCst), 1);

    let path = agent.session_path().expect("session path");
    assert_valid_jsonl(&path);

    // Repeated shutdown on the still-wedged run times out again but must not
    // re-run teardown or duplicate callbacks.
    let repeat = tokio::time::timeout(std::time::Duration::from_secs(10), agent.shutdown())
        .await
        .expect("repeat shutdown is bounded");
    assert!(matches!(repeat, Err(SdkError::Other(_))));
    assert_eq!(shutdowns.load(Ordering::SeqCst), 1, "on_shutdown one-shot");
    assert_eq!(
        plugin_events
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == "session_shutdown")
            .count(),
        1,
        "session_shutdown one-shot"
    );
    assert!(background_ends.lock().unwrap().is_empty());
    assert_valid_jsonl(&path);

    stream_task.abort();
}

/// Registers two commands with the SAME declared name: the registry must
/// expose the plain name plus the deduped `dupe:1` / `dupe:2` spellings, and
/// dispatch through the second spelling must reach the second handler.
struct DuplicateCommandPlugin {
    info: PluginInfo,
    calls: Arc<Mutex<Vec<String>>>,
}

impl AgentPlugin for DuplicateCommandPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        for (label, call_log) in [
            ("first", Arc::clone(&self.calls)),
            ("second", Arc::clone(&self.calls)),
        ] {
            let handler: CommandHandler = Arc::new(move |args: &str| {
                let entry = format!("{label}:{args}");
                let log = Arc::clone(&call_log);
                Box::pin(async move {
                    log.lock().unwrap().push(entry);
                    pir::extensions::CommandOutcome {
                        ok: true,
                        error: None,
                    }
                })
            });
            ctx.register_command(
                "dupe",
                Some("duplicate command"),
                Some(CommandBackend::Rust(handler)),
            );
        }
        Ok(())
    }
}

#[tokio::test]
async fn commands_dedupes_duplicate_registrations_with_numbered_spellings() {
    let temp = TempDir::new("sdk-commands-dedupe");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = agent_builder(
        registry(Arc::new(|_index, _context| {
            assistant(vec![text_block("no run needed")], StopReason::Stop)
        })),
        temp.path(),
    )
    .agent_dir(temp.path().join("agent"))
    .session(SessionMode::None)
    .with_plugin(Arc::new(DuplicateCommandPlugin {
        info: PluginInfo::inline("duplicate-commands"),
        calls: Arc::clone(&calls),
    }))
    .build()
    .await
    .expect("build duplicate-command agent");

    let names: Vec<String> = agent
        .commands()
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| name == "dupe" || name.starts_with("dupe:"))
        .collect();
    assert_eq!(
        names,
        ["dupe", "dupe:1", "dupe:2"],
        "duplicate registrations expose the plain name and numbered spellings"
    );
    let outcome = agent
        .dispatch_command("dupe:2", "args")
        .await
        .expect("dupe:2 dispatches");
    assert!(outcome.ok, "dupe:2 outcome: {outcome:?}");
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["second:args"],
        "dupe:2 reaches the second registration, not the first"
    );
    agent.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn memory_tool_read_only_and_no_js_unknown_command_err() {
    let temp = TempDir::new("sdk-memory-tool-readonly");
    let agent_dir = temp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    let store = temp.path().join("memory-store.cm");
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_vec_pretty(&json!({
            "memory": {
                "store": "path",
                "path": store.to_string_lossy(),
                "write": "readOnly",
            }
        }))
        .expect("serialize settings"),
    )
    .expect("write settings");

    let registry = registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![
                    tool_call_block(
                        "mem-remember",
                        "memory",
                        vec![
                            ("action", json!("remember")),
                            ("kind", json!("fact")),
                            ("subject", json!("tool-readonly")),
                            ("text", json!("mustnotland")),
                        ],
                    ),
                    tool_call_block(
                        "mem-recall",
                        "memory",
                        vec![
                            ("action", json!("recall")),
                            ("query", json!("tool-readonly")),
                        ],
                    ),
                ],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text_block("memory tool done")], StopReason::Stop)
        }
    }));
    let agent = agent_builder(registry, temp.path())
        .agent_dir(agent_dir.clone())
        .session(SessionMode::None)
        .builtin("memory")
        .build()
        .await
        .expect("build memory agent");

    let events = agent
        .prompt_stream_with_outcome("exercise the memory tool under readOnly")
        .collect::<Vec<_>>()
        .await;
    let outcome = lifecycle_outcome(&events);
    let rendered = |result: &pir::ai::types::ToolResultMessage| {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(value) => Some(value.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    };
    let remember = outcome
        .tool_results
        .iter()
        .find(|result| result.tool_call_id == "mem-remember")
        .expect("remember tool result");
    assert!(remember.is_error, "remember is refused: {remember:?}");
    assert!(
        rendered(remember).contains("read-only"),
        "the refusal names readOnly: {remember:?}"
    );
    let recall = outcome
        .tool_results
        .iter()
        .find(|result| result.tool_call_id == "mem-recall")
        .expect("recall tool result");
    assert!(!recall.is_error, "recall stays available: {recall:?}");

    // commands(): sorted, unique invocation spellings (the same contract the
    // dedupe produces for `name` / `name:2` duplicates).
    let commands = agent.commands();
    assert!(
        commands.iter().any(|(name, _)| name == "memory"),
        "memory command is listed: {commands:?}"
    );
    let names: Vec<&str> = commands.iter().map(|(name, _)| name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "commands are sorted by name");
    let mut unique = names.clone();
    unique.dedup();
    assert_eq!(unique, names, "commands have unique invocation names");

    // No JS runtime: an unknown command is a typed Err naming the command.
    let error = agent
        .dispatch_command("no-such-command", "")
        .await
        .expect_err("no-JS unknown command must be an Err");
    let message = error.to_string();
    assert!(
        message.contains("not registered") && message.contains("no-such-command"),
        "the Err names the unregistered command: {message}"
    );
    agent.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn dispatch_command_unknown_and_js_fallback_and_memory_read_only_boundaries() {
    let temp = TempDir::new("sdk-command-boundaries");
    let agent_dir = temp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    let store = temp.path().join("memory-store.cm");
    let write_settings = |agent_dir: &Path, store: &Path, write_mode: Option<&str>| {
        let mut memory = json!({
            "store": "path",
            "path": store.to_string_lossy(),
        });
        if let Some(mode) = write_mode {
            memory["write"] = json!(mode);
        }
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_vec_pretty(&json!({ "memory": memory })).expect("serialize settings"),
        )
        .expect("write settings");
    };
    write_settings(&agent_dir, &store, None);

    let js = temp.path().join("js_command_extension.js");
    std::fs::write(
        &js,
        r#"
        export default function (pi) {
          pi.registerCommand("jshello", { handler: async (args, ctx) => {
            globalThis.__pir_jshello = args;
          } });
        }
        "#,
    )
    .expect("write JS command extension");

    let make_agent = || {
        agent_builder(
            registry(Arc::new(|_i, _c| {
                assistant(vec![text_block("ok")], StopReason::Stop)
            })),
            temp.path(),
        )
        .agent_dir(agent_dir.clone())
        .session(SessionMode::None)
        .extension_paths(vec![js.clone()])
        .with_plugin(Arc::new(MemoryPlugin::with_paths(
            agent_dir.clone(),
            temp.path(),
        )))
    };

    let writer = make_agent().build().await.expect("build writer agent");
    // With a JS runtime present, a name the native registry does not know
    // falls back to the JS runtime's own dispatch table, which reports it as a
    // failed outcome (`ok: false`) instead of an Err.
    let unknown = writer
        .dispatch_command("no-such-command", "")
        .await
        .expect("the JS fallback returns an outcome");
    assert!(
        !unknown.ok,
        "unknown command is a failed outcome: {unknown:?}"
    );
    assert!(
        unknown
            .error
            .as_deref()
            .is_some_and(|text| text.contains("Unknown command")),
        "the fallback names the unknown command: {unknown:?}"
    );
    let js_outcome = writer
        .dispatch_command("jshello", "world")
        .await
        .expect("JS-backed command dispatches");
    assert!(js_outcome.ok, "JS command outcome: {js_outcome:?}");
    let init = writer
        .dispatch_command("memory", "init")
        .await
        .expect("memory init");
    assert!(init.ok, "memory init: {init:?}");
    let remember = writer
        .dispatch_command(
            "memory",
            "remember --kind fact --subject ro-seed --text readonlynote",
        )
        .await
        .expect("memory remember");
    assert!(remember.ok, "memory remember: {remember:?}");
    writer.shutdown().await.expect("shutdown writer");

    // Flip the SAME store to read-only and build a fresh agent: writes are
    // refused through the command path while recall keeps working.
    write_settings(&agent_dir, &store, Some("readOnly"));
    let readonly = make_agent().build().await.expect("build read-only agent");
    let recall = readonly
        .dispatch_command("memory", "recall --query ro-seed")
        .await
        .expect("memory recall");
    assert!(recall.ok, "recall stays available: {recall:?}");
    assert!(
        recall
            .error
            .as_deref()
            .is_some_and(|text| text.contains("readonlynote")),
        "recall reads the seeded record: {recall:?}"
    );
    let refused = readonly
        .dispatch_command(
            "memory",
            "remember --kind fact --subject ro-blocked --text mustnotland",
        )
        .await
        .expect("read-only remember still returns an outcome");
    assert!(!refused.ok, "read-only writes are refused: {refused:?}");
    assert!(
        refused
            .error
            .as_deref()
            .is_some_and(|text| text.contains("read-only")),
        "the refusal names read-only: {refused:?}"
    );
    readonly.shutdown().await.expect("shutdown read-only agent");
}

// ---------------------------------------------------------------------------
// Round 35 — proxy/endpoint build error paths (typed, before any network).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn endpoint_and_model_build_error_paths_are_typed() {
    let temp = TempDir::new("sdk-build-error-paths");

    // 1) Endpoint with a zero window is rejected with the endpoint diagnostic.
    let mut endpoint = pir::sdk::Endpoint::new(
        "http://127.0.0.1:1/v1",
        pir::sdk::ApiKind::OpenAiCompletions,
        "local-model",
    );
    endpoint.context_window = 0;
    let error = AgentBuilder::new()
        .endpoint(endpoint)
        .cwd(temp.path())
        .agent_dir(temp.path().join("agent-a"))
        .build()
        .await
        .expect_err("zero endpoint window must fail");
    assert!(
        error
            .to_string()
            .contains("endpoint context_window and max_tokens must be non-zero"),
        "endpoint diagnostic: {error}"
    );

    // 2) A caller-supplied registry model with a zero window/max_tokens is
    //    rejected with the model diagnostic (the proxy path ends here too).
    let mut model = mock_model();
    model.context_window = 0;
    let error = AgentBuilder::new()
        .with_registry(
            registry(Arc::new(|_index, _context| {
                assistant(vec![text_block("unused")], StopReason::Stop)
            })),
            model,
        )
        .cwd(temp.path())
        .agent_dir(temp.path().join("agent-b"))
        .build()
        .await
        .expect_err("zero model window must fail");
    assert!(
        error
            .to_string()
            .contains("must declare a non-zero context_window and max_tokens"),
        "model diagnostic: {error}"
    );

    // 3) Conflicting setup shapes name both sides instead of silently picking.
    let error = AgentBuilder::new()
        .endpoint(pir::sdk::Endpoint::new(
            "http://127.0.0.1:1/v1",
            pir::sdk::ApiKind::OpenAiCompletions,
            "local-model",
        ))
        .model("scripted-model")
        .cwd(temp.path())
        .agent_dir(temp.path().join("agent-c"))
        .build()
        .await
        .expect_err("conflicting setups must fail");
    let message = error.to_string();
    assert!(
        message.contains("conflicting model configuration")
            && message.contains("endpoint()")
            && message.contains("model()"),
        "conflict diagnostic: {message}"
    );
}

// ---------------------------------------------------------------------------
// Round 34 — Agent::context_usage() session-backed stats (PIR-SDK-011), and
// Agent::set_tool_hooks runtime replacement.
// ---------------------------------------------------------------------------

/// Calls `Agent::context_usage()` from inside a real tool execution (mid-run,
/// after the assistant message with the usage record was persisted).
struct UsageProbeTool {
    probes: Arc<Mutex<Vec<Option<pir::sdk::ContextUsage>>>>,
    agent: Arc<std::sync::OnceLock<pir::sdk::Agent>>,
}

impl AgentTool for UsageProbeTool {
    fn name(&self) -> &'static str {
        "usage_probe"
    }

    fn description(&self) -> &'static str {
        "reads the live context usage"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object"}))
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let probes = Arc::clone(&self.probes);
        let agent = self.agent.get().cloned();
        Box::pin(async move {
            let usage = match agent {
                Some(agent) => agent.context_usage().await,
                None => None,
            };
            probes.lock().unwrap().push(usage);
            ToolResult::ok("probed")
        })
    }
}

#[tokio::test]
async fn context_usage_reports_session_backed_live_usage_and_compaction_rebuild() {
    let temp = TempDir::new("sdk-context-usage");
    let probes = Arc::new(Mutex::new(Vec::new()));
    let agent_slot = Arc::new(std::sync::OnceLock::new());
    let registry = registry(Arc::new(|index, context| {
        let is_summary = context
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.starts_with("You are a context summarization assistant"));
        if is_summary {
            let mut message = assistant(vec![text_block("compacted summary")], StopReason::Stop);
            message.usage = Usage {
                input: 10,
                output: 2,
                total_tokens: 12,
                ..Usage::default()
            };
            return message;
        }
        match index {
            0 => {
                let mut message = assistant(
                    vec![tool_call("usage-call", "usage_probe")],
                    StopReason::ToolUse,
                );
                message.usage = Usage {
                    input: 19_000,
                    output: 1,
                    total_tokens: 19_001,
                    ..Usage::default()
                };
                message
            }
            1 => {
                let mut message = assistant(vec![text_block("turn two")], StopReason::Stop);
                message.usage = Usage {
                    input: 40,
                    output: 4,
                    total_tokens: 44,
                    ..Usage::default()
                };
                message
            }
            _ => {
                let mut message = assistant(vec![text_block("after compaction")], StopReason::Stop);
                message.usage = Usage {
                    input: 10,
                    output: 1,
                    total_tokens: 11,
                    ..Usage::default()
                };
                message
            }
        }
    }));
    let mut model = mock_model();
    model.context_window = 200_000;
    model.max_tokens = 8_000;
    let agent = AgentBuilder::new()
        .with_registry(Arc::clone(&registry), model)
        .cwd(temp.path())
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .agent_dir(temp.path().join("agent"))
        .session(SessionMode::Dir(temp.path().join("sessions")))
        .compaction(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 2_000,
            keep_recent_tokens: 1,
        })
        .with_tool(Arc::new(UsageProbeTool {
            probes: Arc::clone(&probes),
            agent: Arc::clone(&agent_slot),
        }))
        .build()
        .await
        .expect("build context-usage agent");
    let _ = agent_slot.set(agent.clone());

    let outcome = agent.prompt("probe the live usage").await.expect("prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("turn two"));

    // Mid-run probe: the session holds the assistant message with 19_001 usage
    // before the tool runs, and the stats surface reports it.
    let probe = probes.lock().unwrap()[0]
        .clone()
        .expect("mid-run context usage is available");
    let tokens = probe.tokens.expect("mid-run tokens");
    assert!(
        tokens >= 19_001,
        "context_usage includes the live usage record: {probe:?}"
    );
    assert_eq!(probe.context_window, 200_000);

    // End of run: the newest assistant usage (44) is authoritative.
    let usage = agent.context_usage().await.expect("usage available");
    assert_eq!(usage.tokens, Some(44), "latest usage wins: {usage:?}");

    // Manual compaction: the last usage predates the compaction, so the stats
    // surface reports an UNKNOWN count (upstream getContextUsage).
    agent.compact().await.expect("manual compaction");
    let after = agent.context_usage().await.expect("usage available");
    assert_eq!(
        after.tokens, None,
        "a compaction invalidates the pre-compaction usage: {after:?}"
    );

    // The next run re-establishes a usage basis for the rebuilt context.
    let outcome = agent.prompt("after compaction").await.expect("prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("after compaction"));
    let rebuilt = agent.context_usage().await.expect("usage available");
    assert_eq!(rebuilt.tokens, Some(11), "rebuilt usage: {rebuilt:?}");
    agent.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn set_tool_hooks_replaces_the_installed_handle_at_runtime() {
    let temp = TempDir::new("sdk-set-tool-hooks");
    let alternator = Arc::new(AtomicUsize::new(0));
    let registry = registry(Arc::new({
        let alternator = Arc::clone(&alternator);
        move |_index, _context| {
            if alternator.fetch_add(1, Ordering::SeqCst) % 2 == 0 {
                assistant(
                    vec![tool_call("fast-call", "fast_tool")],
                    StopReason::ToolUse,
                )
            } else {
                assistant(vec![text_block("done")], StopReason::Stop)
            }
        }
    }));
    let agent = agent_builder(registry, temp.path())
        .agent_dir(temp.path().join("agent"))
        .session(SessionMode::None)
        .no_compaction()
        .with_tool(Arc::new(FastTool))
        .build()
        .await
        .expect("build tool-hooks agent");

    let hook_a = Arc::new(Mutex::new(Vec::<String>::new()));
    let hook_b = Arc::new(Mutex::new(Vec::<String>::new()));
    let make_hooks = |log: Arc<Mutex<Vec<String>>>, label: &'static str| {
        Some(Arc::new(pir::agent::tool_execution::ToolHooks {
            before: Some(Box::new(move |call, _ctx| {
                let log = Arc::clone(&log);
                let entry = format!("{label}:{}", call.tool_name);
                Box::pin(async move {
                    log.lock().unwrap().push(entry);
                    pir::agent::tool_execution::BeforeToolCallResult::default()
                })
            })),
            after: None,
        }))
    };

    agent.set_tool_hooks(make_hooks(Arc::clone(&hook_a), "A"));
    assert!(agent.tool_hooks().is_some());
    agent.prompt("first run").await.expect("first prompt");
    assert_eq!(hook_a.lock().unwrap().as_slice(), ["A:fast_tool"]);

    agent.set_tool_hooks(make_hooks(Arc::clone(&hook_b), "B"));
    agent.prompt("second run").await.expect("second prompt");
    assert_eq!(
        hook_a.lock().unwrap().as_slice(),
        ["A:fast_tool"],
        "replaced hooks do not run again"
    );
    assert_eq!(
        hook_b.lock().unwrap().as_slice(),
        ["B:fast_tool"],
        "the replacement hook runs on the next run"
    );

    agent.set_tool_hooks(None);
    assert!(agent.tool_hooks().is_none());
    agent.prompt("third run").await.expect("third prompt");
    assert_eq!(hook_a.lock().unwrap().len(), 1);
    assert_eq!(hook_b.lock().unwrap().len(), 1);
    agent.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Round 33 — PIR-SDK-011: the estimator/projection token count must include a
// usage record that lives in the live tail (the stats surface uses the same
// function). Direct estimator assertions, not a compaction side effect.
// ---------------------------------------------------------------------------

#[test]
fn projection_estimator_includes_live_tail_usage_across_stats_cases() {
    use pir::compaction::{
        estimate_projected_context_tokens_with_tail, CutEntryRef, PathEntryMeta,
    };

    let usage_message = |total: u64, timestamp: u64| {
        let mut message = assistant(vec![text_block("answer")], StopReason::Stop);
        message.usage = Usage {
            input: total.saturating_sub(1),
            output: 1,
            total_tokens: total,
            ..Usage::default()
        };
        message.timestamp = timestamp;
        message
    };

    // (a) No usage anywhere: the estimate is content-only.
    let no_usage = vec![Message::Assistant(assistant(
        vec![text_block("plain")],
        StopReason::Stop,
    ))];
    let no_usage_refs: Vec<CutEntryRef<'_>> =
        no_usage.iter().map(CutEntryRef::from_message).collect();
    let empty_projected: Vec<CutEntryRef<'_>> = Vec::new();
    let no_path: &[PathEntryMeta] = &[];
    let estimate = estimate_projected_context_tokens_with_tail(&no_usage_refs, &[], no_path);
    assert_eq!(estimate.usage_tokens, 0, "no usage record: {estimate:?}");
    assert_eq!(estimate.last_usage_index, None);

    // (b) A usage record in the live tail is authoritative.
    let usage_tail = vec![Message::Assistant(usage_message(19_001, 10))];
    let estimate =
        estimate_projected_context_tokens_with_tail(&empty_projected, &usage_tail, no_path);
    assert_eq!(
        estimate.usage_tokens, 19_001,
        "the tail usage is counted: {estimate:?}"
    );
    assert_eq!(estimate.last_usage_index, Some(0));
    assert!(
        estimate.tokens >= 19_001,
        "tokens include the usage record: {estimate:?}"
    );

    // (c) Multiple tail messages after the usage: trailing content is added
    // on top of the usage.
    let multi_tail = vec![
        Message::Assistant(usage_message(19_001, 10)),
        Message::Assistant(assistant(
            vec![text_block(&"y".repeat(4_000))],
            StopReason::Stop,
        )),
    ];
    let estimate =
        estimate_projected_context_tokens_with_tail(&empty_projected, &multi_tail, no_path);
    assert_eq!(estimate.usage_tokens, 19_001);
    assert!(
        estimate.trailing_tokens > 0,
        "messages after the usage are estimated: {estimate:?}"
    );
    assert!(
        estimate.tokens >= 19_001 + estimate.trailing_tokens,
        "usage + trailing: {estimate:?}"
    );

    // (d) A compaction on the branch BEFORE the tail does not invalidate the
    // tail usage (the record is newer than every branch entry).
    let after_compaction = [PathEntryMeta {
        id: Some("compaction-1".to_owned()),
        is_compaction: true,
        is_context_edit: false,
    }];
    let estimate = estimate_projected_context_tokens_with_tail(
        &empty_projected,
        &usage_tail,
        &after_compaction,
    );
    assert_eq!(
        estimate.usage_tokens, 19_001,
        "tail usage survives an older compaction: {estimate:?}"
    );

    // (e) Rebuild with a later compaction: a projected usage without a branch
    // identity is stale and must fall back to content estimation.
    let projected_only = vec![Message::Assistant(usage_message(25_000, 5))];
    let projected_refs: Vec<CutEntryRef<'_>> = projected_only
        .iter()
        .map(CutEntryRef::from_message)
        .collect();
    let estimate =
        estimate_projected_context_tokens_with_tail(&projected_refs, &[], &after_compaction);
    assert_eq!(
        estimate.usage_tokens, 0,
        "usage with a compaction after it is stale: {estimate:?}"
    );
}

// ---------------------------------------------------------------------------
// Round 26 — PIR-SDK-010: mid-run set_model must reach subagent dispatch
// defaults and the compaction threshold/summarizer of the SAME run.
// ---------------------------------------------------------------------------

/// Pauses its first stream after one delta, then answers by call index.
/// Records `(model id, thinking level, is_summarization)` per request.
struct PausedRunLifecycleProvider {
    calls: AtomicUsize,
    seen: Arc<Mutex<Vec<(String, Option<String>, bool)>>>,
    release: tokio::sync::watch::Receiver<bool>,
    /// `"subagent"` asks the first turn for the real subagent builtin;
    /// `"fast"` asks for `fast_tool` (the compaction scenario).
    first_tool: &'static str,
    /// Input usage attributed to the first assistant message.
    first_input_tokens: u64,
}

impl Provider for PausedRunLifecycleProvider {
    fn id(&self) -> &str {
        "scripted-run-lifecycle"
    }

    fn name(&self) -> &str {
        "Paused run lifecycle provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![
            paused_lifecycle_model("scripted-run-lifecycle", "alpha-model", 200_000, 8_000),
            paused_lifecycle_model("scripted-run-lifecycle", "delta-model", 20_000, 4_000),
        ]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let is_summary = context
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.starts_with("You are a context summarization assistant"));
        self.seen.lock().unwrap().push((
            model.id.clone(),
            options.reasoning.map(|level| level.wire_key().to_owned()),
            is_summary,
        ));
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        let producer = stream.clone();
        let mut release = self.release.clone();
        let model_id = model.id.clone();
        let provider_id = model.provider.clone();
        let first_tool = self.first_tool;
        let first_input_tokens = self.first_input_tokens;
        tokio::spawn(async move {
            if index == 0 {
                let mut partial = assistant(vec![text_block("working")], StopReason::Stop);
                partial.provider = provider_id.clone();
                partial.model = model_id.clone();
                producer.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "working".to_owned(),
                    partial: Arc::new(partial),
                });
                let _ = release.changed().await;
                let mut message = if first_tool == "subagent" {
                    assistant(
                        vec![tool_call_block(
                            "paused-subagent",
                            "subagent",
                            vec![("agent", json!("scout")), ("task", json!("child task"))],
                        )],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(
                        vec![tool_call("paused-tool", "fast_tool")],
                        StopReason::ToolUse,
                    )
                };
                message.provider = provider_id;
                message.model = model_id;
                message.usage = Usage {
                    input: first_input_tokens,
                    output: 1,
                    total_tokens: first_input_tokens + 1,
                    ..Usage::default()
                };
                // A real provider stamps the response after the prompt; the
                // usage record is only authoritative when its timestamp is
                // not older than the preceding prefix messages.
                message.timestamp = pir::ai::util::now_ms();
                producer.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    message,
                ));
            } else {
                let text = if is_summary {
                    "summary of the conversation".to_owned()
                } else if index == 1 && first_tool == "subagent" {
                    "child done".to_owned()
                } else {
                    "final done".to_owned()
                };
                let mut message = assistant(vec![text_block(&text)], StopReason::Stop);
                message.provider = provider_id;
                message.model = model_id;
                message.usage = Usage {
                    input: 40,
                    output: 4,
                    total_tokens: 44,
                    ..Usage::default()
                };
                producer.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: text.clone(),
                    partial: Arc::new(message.clone()),
                });
                producer.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    message,
                ));
            }
        });
        stream
    }
}

fn paused_lifecycle_model(
    provider_id: &str,
    model_id: &str,
    context_window: u64,
    max_tokens: u64,
) -> Model {
    let mut model = named_model(provider_id, model_id);
    model.reasoning = true;
    model.context_window = context_window;
    model.max_tokens = max_tokens;
    model
}

fn event_outcome_text(events: &[SdkEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        SdkEvent::AgentOutcome(outcome) => outcome.message_text(),
        _ => None,
    })
}

fn lifecycle_outcome(events: &[SdkEvent]) -> &pir::sdk::AgentOutcome {
    events
        .iter()
        .find_map(|event| match event {
            SdkEvent::AgentOutcome(outcome) => Some(outcome),
            _ => None,
        })
        .expect("an AgentOutcome event")
}

/// PIR-SDK-010 real repro (subagent half): the run starts on `alpha-model`
/// with thinking off; while turn 1 is still streaming the test calls
/// `set_model(delta-model)` + `set_thinking_level(high)`. The same turn then
/// calls the REAL `subagent` builtin with no model override, and turn 2
/// finishes. The provider must see the child request and turn 2 on
/// delta-model/high; the child transcript (carried in the tool result details
/// because in-process children are ephemeral, upstream `--no-session`) must
/// attribute its assistant message to delta-model.
#[tokio::test]
async fn subagent_dispatch_uses_live_model_and_thinking_after_mid_stream_switch() {
    let temp = TempDir::new("sdk-subagent-live-model");
    let agents_dir = temp.path().join("agent").join("agents");
    std::fs::create_dir_all(&agents_dir).expect("create agents dir");
    std::fs::write(
        agents_dir.join("scout.md"),
        "---\nname: scout\ndescription: Answers briefly\n---\nBe useful.\n",
    )
    .expect("write scout fixture");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let mut reg = ProviderRegistry::default();
    reg.set_provider(Arc::new(PausedRunLifecycleProvider {
        calls: AtomicUsize::new(0),
        seen: Arc::clone(&seen),
        release: release_rx,
        first_tool: "subagent",
        first_input_tokens: 10,
    }));
    let agent = AgentBuilder::new()
        .with_registry(
            Arc::new(reg),
            paused_lifecycle_model("scripted-run-lifecycle", "alpha-model", 200_000, 8_000),
        )
        .cwd(temp.path())
        .agent_dir(temp.path().join("agent"))
        .session(SessionMode::Dir(temp.path().join("sessions")))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .builtin("subagent")
        .build()
        .await
        .expect("build parent session with the real subagent builtin");

    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn({
        let agent = agent.clone();
        async move {
            let mut events = Vec::new();
            let stream = agent.prompt_stream_with_outcome("delegate");
            futures_util::pin_mut!(stream);
            let mut announced = false;
            let mut first_tx = Some(first_tx);
            while let Some(event) = stream.next().await {
                if !announced
                    && matches!(&event, SdkEvent::MessageUpdate { delta, .. } if delta == "working")
                {
                    announced = true;
                    if let Some(tx) = first_tx.take() {
                        let _ = tx.send(());
                    }
                }
                events.push(event);
            }
            events
        }
    });

    first_rx.await.expect("first delta observed");
    assert_eq!(agent.model().id, "alpha-model");
    agent
        .set_model(paused_lifecycle_model(
            "scripted-run-lifecycle",
            "delta-model",
            20_000,
            4_000,
        ))
        .await
        .expect("switch model mid-stream");
    agent
        .set_thinking_level(ThinkingLevel::High)
        .await
        .expect("switch thinking mid-stream");
    release_tx.send(true).expect("release paused stream");

    let events = task.await.expect("join stream");
    let debug = format!("{events:?}");
    assert_eq!(
        event_outcome_text(&events).as_deref(),
        Some("final done"),
        "the parent run completes on the new model: {debug}"
    );

    // Real subagent evidence: the tool result carries the child transcript,
    // whose assistant message names the model the child actually streamed on.
    let outcome = lifecycle_outcome(&events);
    let subagent_result = outcome
        .tool_results
        .iter()
        .find(|result| result.tool_name == "subagent")
        .expect("the real subagent tool ran");
    assert!(
        !subagent_result.is_error,
        "subagent call must succeed: {subagent_result:?}"
    );
    let details = subagent_result.details.as_ref().expect("subagent details");
    let child_messages = details["results"][0]["messages"]
        .as_array()
        .expect("child transcript in details");
    let child_assistant_models: Vec<&str> = child_messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .map(|message| message["model"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        child_assistant_models,
        ["delta-model"],
        "the in-process child streams on the live parent model: {details}"
    );

    let seen = seen.lock().unwrap().clone();
    let models: Vec<&str> = seen.iter().map(|(model, _, _)| model.as_str()).collect();
    assert_eq!(
        models,
        ["alpha-model", "delta-model", "delta-model"],
        "parent turn 1, real child, parent turn 2 must all use the expected models: {seen:?}"
    );
    let thinking: Vec<Option<&str>> = seen
        .iter()
        .map(|(_, thinking, _)| thinking.as_deref())
        .collect();
    assert_eq!(
        thinking,
        [Some("off"), Some("high"), Some("high")],
        "the child inherits the live thinking level, not the run-start one: {seen:?}"
    );

    // The parent session JSONL keeps the switch: turn 1 alpha, turn 2 delta.
    let path = agent.session_path().expect("session path");
    let entries = assert_valid_jsonl(&path);
    let parent_models: Vec<String> = entries
        .iter()
        .filter(|entry| entry["type"] == "message")
        .filter(|entry| entry["message"]["role"] == "assistant")
        .map(|entry| {
            entry["message"]["model"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(parent_models, ["alpha-model", "delta-model"]);
    agent.shutdown().await.expect("shutdown agent");
}

/// Returns a conversation-sized payload: the NEXT turn's request content must
/// cross the 20k model's threshold regardless of usage accounting. This is
/// what makes the window switch observable from the request itself.
struct LargeFastTool;

impl AgentTool for LargeFastTool {
    fn name(&self) -> &'static str {
        "fast_tool"
    }

    fn description(&self) -> &'static str {
        "returns a conversation-sized payload"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object"}))
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("x".repeat(75_000)) })
    }
}

/// Builds the compaction scenario: first turn streams (paused), then calls
/// `fast_tool`, whose large result makes the SAME run's next request exceed a
/// 20k window so the window switch is observable whether it happened or not.
/// the SAME run reaches another threshold check after the switch.
async fn run_compaction_switch(
    temp: &TempDir,
    start_model: &'static str,
    switched_model: &'static str,
    first_input_tokens: u64,
    large_tool: bool,
) -> (
    Vec<(String, Option<String>, bool)>,
    Vec<SdkEvent>,
    pir::sdk::Agent,
) {
    let start_window = if start_model == "alpha-model" {
        200_000
    } else {
        20_000
    };
    let start_max = if start_model == "alpha-model" {
        8_000
    } else {
        4_000
    };
    let switched_window = if switched_model == "alpha-model" {
        200_000
    } else {
        20_000
    };
    let switched_max = if switched_model == "alpha-model" {
        8_000
    } else {
        4_000
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let mut reg = ProviderRegistry::default();
    reg.set_provider(Arc::new(PausedRunLifecycleProvider {
        calls: AtomicUsize::new(0),
        seen: Arc::clone(&seen),
        release: release_rx,
        first_tool: "fast",
        // `100` makes the window switch observable through request CONTENT (a
        // large tool result); `19_000` exercises the in-run usage record.
        first_input_tokens,
    }));
    let agent = AgentBuilder::new()
        .with_registry(
            Arc::new(reg),
            paused_lifecycle_model(
                "scripted-run-lifecycle",
                start_model,
                start_window,
                start_max,
            ),
        )
        .cwd(temp.path())
        .agent_dir(temp.path().join("agent"))
        .no_session()
        .compaction(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 2_000,
            keep_recent_tokens: 1,
        })
        .thinking_level(ThinkingLevel::Off)
        .with_tool(if large_tool {
            Arc::new(LargeFastTool) as Arc<dyn AgentTool>
        } else {
            Arc::new(FastTool) as Arc<dyn AgentTool>
        })
        .build()
        .await
        .expect("build compaction agent");

    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn({
        let agent = agent.clone();
        async move {
            let mut events = Vec::new();
            let stream = agent.prompt_stream_with_outcome("a deliberately long task description");
            futures_util::pin_mut!(stream);
            let mut announced = false;
            let mut first_tx = Some(first_tx);
            while let Some(event) = stream.next().await {
                if !announced
                    && matches!(&event, SdkEvent::MessageUpdate { delta, .. } if delta == "working")
                {
                    announced = true;
                    if let Some(tx) = first_tx.take() {
                        let _ = tx.send(());
                    }
                }
                events.push(event);
            }
            events
        }
    });

    first_rx.await.expect("first delta observed");
    agent
        .set_model(paused_lifecycle_model(
            "scripted-run-lifecycle",
            switched_model,
            switched_window,
            switched_max,
        ))
        .await
        .expect("switch model mid-stream");
    release_tx.send(true).expect("release paused stream");
    let events = task.await.expect("join stream");
    let seen = seen.lock().unwrap().clone();
    (seen, events, agent)
}

/// PIR-SDK-010 real repro (compaction half, shrinking window): the first turn
/// reports ~19k input tokens under a 200k window. Mid-stream the model
/// switches to one with a 20k window / 2k reserve, so the SAME run's next
/// threshold check must compact and the summarizer must call `delta-model`.
#[tokio::test]
async fn compaction_mid_stream_switch_to_smaller_window_compacts_with_new_model() {
    let temp = TempDir::new("sdk-compaction-live-small");
    let (seen, events, agent) =
        run_compaction_switch(&temp, "alpha-model", "delta-model", 100, true).await;
    assert_eq!(
        event_outcome_text(&events).as_deref(),
        Some("final done"),
        "compaction must not break the run: {events:?}"
    );
    let summaries: Vec<&(String, Option<String>, bool)> =
        seen.iter().filter(|(_, _, summary)| *summary).collect();
    assert_eq!(
        summaries.len(),
        1,
        "exactly one summarizer request, on the live model: {seen:?}"
    );
    assert_eq!(summaries[0].0, "delta-model");
    let models: Vec<&str> = seen.iter().map(|(model, _, _)| model.as_str()).collect();
    assert_eq!(
        models,
        ["alpha-model", "delta-model", "delta-model"],
        "run-start request, live summarizer, live next turn: {seen:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SdkEvent::Compaction)),
        "the threshold compaction must run: {events:?}"
    );
    agent.shutdown().await.expect("shutdown");
}

/// PIR-SDK-011: the usage record produced by the run's OWN turn lives in the
/// live tail, not in the prompt-start projection. Upstream estimates the
/// projection's messages, where that assistant is already a persisted entry,
/// so its usage counts; pir must not discard the tail record. With tiny
/// content, only the 19k usage can cross the 20k window's 18k threshold.
#[tokio::test]
async fn compaction_mid_stream_switch_uses_in_run_tail_usage() {
    let temp = TempDir::new("sdk-compaction-tail-usage");
    let (seen, events, agent) =
        run_compaction_switch(&temp, "alpha-model", "delta-model", 19_000, false).await;
    assert_eq!(
        event_outcome_text(&events).as_deref(),
        Some("final done"),
        "compaction must not break the run: {events:?}"
    );
    let summaries: Vec<&(String, Option<String>, bool)> =
        seen.iter().filter(|(_, _, summary)| *summary).collect();
    assert_eq!(
        summaries.len(),
        1,
        "the in-run usage record must trigger the shrinking-window compaction: {seen:?}"
    );
    assert_eq!(summaries[0].0, "delta-model");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SdkEvent::Compaction)),
        "the threshold compaction must run: {events:?}"
    );
    agent.shutdown().await.expect("shutdown");
}

/// PIR-SDK-010 compaction half, growing window: the run starts on the 20k
/// model (its own threshold would fire at ~19k usage) and switches to the
/// 200k model mid-stream. The next turn must NOT compact — nothing is near
/// the live window.
#[tokio::test]
async fn compaction_mid_stream_switch_to_larger_window_does_not_compact() {
    let temp = TempDir::new("sdk-compaction-live-large");
    let (seen, events, agent) =
        run_compaction_switch(&temp, "delta-model", "alpha-model", 100, true).await;
    assert_eq!(event_outcome_text(&events).as_deref(), Some("final done"));
    assert!(
        !seen.iter().any(|(_, _, summary)| *summary),
        "no compaction may run under the larger live window: {seen:?}"
    );
    let models: Vec<&str> = seen.iter().map(|(model, _, _)| model.as_str()).collect();
    assert_eq!(models, ["delta-model", "alpha-model"]);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SdkEvent::Compaction)),
        "no compaction may run: {events:?}"
    );
    agent.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Round 39 — session_start lifecycle across attach/new/default modes
// ---------------------------------------------------------------------------

fn context_user_texts(context: &Context) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
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
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn session_start_modes_are_ordered_single_and_carry_upstream_payloads() {
    let temp = TempDir::new("session-start-modes");
    let cwd = temp.path();
    let source_dir = cwd.join("source-sessions");
    let agent_dir = cwd.join("agent");
    let fork_dir = cwd.join("forks");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&fork_dir).unwrap();

    // Two real session files produced by real (scripted-model) runs.
    let seed_session = |question: &str, reply: &str, dir: &Path| {
        let registry = registry(Arc::new({
            let reply = reply.to_owned();
            move |_index, _context| assistant(vec![text_block(&reply)], StopReason::Stop)
        }));
        let builder = agent_builder(registry, cwd)
            .agent_dir(&agent_dir)
            .session(SessionMode::Dir(dir.to_path_buf()));
        let question = question.to_owned();
        async move {
            let agent = builder.build().await.expect("build seed agent");
            agent.prompt(question).await.expect("seed prompt");
            let path = agent.session_path().expect("seed session path");
            agent.shutdown().await.expect("seed shutdown");
            path
        }
    };
    let source_a = seed_session("alpha question", "alpha reply", &source_dir).await;
    let source_b = seed_session("omega question", "omega reply", &source_dir).await;

    // Observe every lifecycle frame that can precede/follow session_start.
    let log: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_contexts: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let record_contexts = Arc::clone(&seen_contexts);
    let registry = registry(Arc::new(move |_index, context| {
        record_contexts.lock().unwrap().push(context.clone());
        assistant(vec![text_block("follow-up reply")], StopReason::Stop)
    }));
    let mut builder = agent_builder(registry, cwd).agent_dir(&agent_dir);
    for name in [
        "session_before_switch",
        "session_shutdown",
        "session_start",
        "input",
        "before_agent_start",
        "message_start",
    ] {
        let log = Arc::clone(&log);
        builder = builder.on_event(name, move |payload| {
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((name.to_owned(), payload.clone()));
        });
    }
    let agent = builder.build().await.expect("build recording agent");

    // 1. DEFAULT mode (SessionMode::DefaultDir): one `startup` on the first
    // prompt, before `input`/`before_agent_start`, and nothing before it.
    assert!(
        log.lock().unwrap().is_empty(),
        "building must not emit session_start"
    );
    agent
        .prompt("default question")
        .await
        .expect("default prompt");
    let events = log.lock().unwrap().clone();
    let starts: Vec<&(String, serde_json::Value)> = events
        .iter()
        .filter(|(name, _)| name == "session_start")
        .collect();
    assert_eq!(
        starts.len(),
        1,
        "exactly one startup session_start: {events:?}"
    );
    assert_eq!(starts[0].1["type"], "session_start");
    assert_eq!(starts[0].1["reason"], "startup");
    assert!(
        starts[0].1.get("previousSessionFile").is_none(),
        "startup has no previousSessionFile: {}",
        starts[0].1
    );
    let pos = |name: &str| {
        events
            .iter()
            .position(|(event, _)| event == name)
            .unwrap_or_else(|| panic!("missing {name} in {events:?}"))
    };
    assert!(
        pos("session_start") < pos("input"),
        "session_start precedes input"
    );
    assert!(
        pos("input") < pos("before_agent_start"),
        "input precedes before_agent_start"
    );
    let default_path = agent.session_path().expect("default session path");
    assert!(default_path.exists(), "the default session was persisted");

    // 2. NEW mode (Agent::new_session): one `new` immediately, previous file
    // named; the following prompt must not replay a second startup.
    let before_new = log.lock().unwrap().len();
    agent.new_session().await.expect("new session");
    let new_events = log.lock().unwrap()[before_new..].to_vec();
    let new_starts: Vec<&(String, serde_json::Value)> = new_events
        .iter()
        .filter(|(name, _)| name == "session_start")
        .collect();
    assert_eq!(
        new_starts.len(),
        1,
        "one session_start for new: {new_events:?}"
    );
    assert_eq!(new_starts[0].1["reason"], "new");
    assert_eq!(
        new_starts[0].1["previousSessionFile"].as_str(),
        Some(default_path.to_string_lossy().as_ref()),
        "new names the outgoing session file"
    );
    let new_pos = |name: &str| {
        new_events
            .iter()
            .position(|(event, _)| event == name)
            .unwrap_or_else(|| panic!("missing {name} in {new_events:?}"))
    };
    assert!(new_pos("session_before_switch") < new_pos("session_shutdown"));
    assert!(new_pos("session_shutdown") < new_pos("session_start"));
    let new_path = agent.session_path().expect("new session path");
    let before_prompt = log.lock().unwrap().len();
    agent.prompt("new question").await.expect("new prompt");
    let after_new_prompt = log.lock().unwrap().clone();
    assert!(
        !after_new_prompt[before_prompt..]
            .iter()
            .any(|(name, _)| name == "session_start"),
        "the new session's first prompt must not emit startup again"
    );
    let texts = context_user_texts(seen_contexts.lock().unwrap().last().unwrap());
    assert_eq!(
        texts,
        ["new question"],
        "a new session starts from an empty transcript"
    );

    // 3. ATTACH mode (Agent::attach_session): one `resume`, then the replayed
    // transcript reaches the provider and no second session_start is sent.
    let before_attach = log.lock().unwrap().len();
    agent
        .attach_session(&source_a)
        .await
        .expect("attach source a");
    let attach_events = log.lock().unwrap()[before_attach..].to_vec();
    let attach_starts: Vec<&(String, serde_json::Value)> = attach_events
        .iter()
        .filter(|(name, _)| name == "session_start")
        .collect();
    assert_eq!(
        attach_starts.len(),
        1,
        "one session_start for attach: {attach_events:?}"
    );
    assert_eq!(attach_starts[0].1["reason"], "resume");
    assert_eq!(
        attach_starts[0].1["previousSessionFile"].as_str(),
        Some(new_path.to_string_lossy().as_ref()),
        "resume names the outgoing session file"
    );
    let before_attach_prompt = log.lock().unwrap().len();
    agent
        .prompt("attached follow-up")
        .await
        .expect("attached prompt");
    let after_attach_prompt = log.lock().unwrap().clone();
    assert!(
        !after_attach_prompt[before_attach_prompt..]
            .iter()
            .any(|(name, _)| name == "session_start"),
        "the resumed session's first prompt must not emit startup again"
    );
    let texts = context_user_texts(seen_contexts.lock().unwrap().last().unwrap());
    assert_eq!(
        texts,
        ["alpha question", "attached follow-up"],
        "attach replays the source history before the new prompt"
    );

    // 4. FORK: attaching a SessionManager::fork_from_source file while the
    // outgoing session is its parent classifies as `fork`, and the fork's
    // transcript is the source branch (not the modified live session).
    agent
        .attach_session(&source_b)
        .await
        .expect("attach source b");
    let before_fork = log.lock().unwrap().len();
    let fork = SessionManager::fork_from_source(
        &source_b,
        cwd.to_string_lossy().as_ref(),
        &fork_dir,
        None,
    )
    .expect("fork source b");
    let fork_path = fork.path().to_path_buf();
    agent.attach_session(&fork_path).await.expect("attach fork");
    let fork_events = log.lock().unwrap()[before_fork..].to_vec();
    let fork_starts: Vec<&(String, serde_json::Value)> = fork_events
        .iter()
        .filter(|(name, _)| name == "session_start")
        .collect();
    assert_eq!(
        fork_starts.len(),
        1,
        "one session_start for fork: {fork_events:?}"
    );
    assert_eq!(fork_starts[0].1["reason"], "fork");
    assert_eq!(
        fork_starts[0].1["previousSessionFile"].as_str(),
        Some(source_b.to_string_lossy().as_ref()),
        "fork names its parent session file"
    );
    let before_fork_prompt = log.lock().unwrap().len();
    agent.prompt("fork follow-up").await.expect("fork prompt");
    let after_fork_prompt = log.lock().unwrap().clone();
    assert!(
        !after_fork_prompt[before_fork_prompt..]
            .iter()
            .any(|(name, _)| name == "session_start"),
        "the forked session's first prompt must not emit startup again"
    );
    let texts = context_user_texts(seen_contexts.lock().unwrap().last().unwrap());
    assert_eq!(
        texts,
        ["omega question", "fork follow-up"],
        "the fork carries only its source branch history"
    );

    agent.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Round 39 — ProxyProvider over a real loopback HTTP SSE server
// ---------------------------------------------------------------------------

fn proxy_model() -> Model {
    let mut model = mock_model();
    model.id = "gpt-test".into();
    model.name = "Proxy test model".into();
    model.api = "openai-responses".into();
    model.provider = "proxy".into();
    model.base_url = "http://127.0.0.1:1".into();
    model
}

struct CapturedProxyRequest {
    request_line: String,
    headers: std::collections::BTreeMap<String, String>,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut std::net::TcpStream) -> CapturedProxyRequest {
    use std::io::Read;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("set read timeout");
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).expect("read request headers");
        assert!(read > 0, "connection closed before the request headers");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_owned();
    let mut headers = std::collections::BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).expect("read request body");
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    CapturedProxyRequest {
        request_line,
        headers,
        body,
    }
}

/// One-connection loopback SSE server: records the request, then writes each
/// event as one chunked `data:` frame. `tail_delay` holds the connection open
/// after the listed events (the abort scenario).
fn spawn_sse_server(
    events: Vec<serde_json::Value>,
    tail_delay: std::time::Duration,
) -> (u16, std::sync::mpsc::Receiver<CapturedProxyRequest>) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback sse server");
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let request = read_http_request(&mut stream);
        let _ = sender.send(request);
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n";
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        for event in events {
            let data = format!("data: {event}\n\n");
            let frame = format!("{:X}\r\n", data.len());
            if stream.write_all(frame.as_bytes()).is_err()
                || stream.write_all(data.as_bytes()).is_err()
                || stream.write_all(b"\r\n").is_err()
            {
                return;
            }
            let _ = stream.flush();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !tail_delay.is_zero() {
            std::thread::sleep(tail_delay);
        }
        let _ = stream.write_all(b"0\r\n\r\n");
        let _ = stream.flush();
    });
    (port, receiver)
}

fn proxy_usage() -> serde_json::Value {
    json!({
        "input": 3,
        "output": 2,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 5,
        "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0 }
    })
}

#[tokio::test]
async fn proxy_prompt_stream_over_loopback_sse_routes_request_and_usage() {
    let temp = TempDir::new("proxy-stream");
    let (port, requests) = spawn_sse_server(
        vec![
            json!({"type": "start"}),
            json!({"type": "text_start", "contentIndex": 0}),
            json!({"type": "text_delta", "contentIndex": 0, "delta": "Hello"}),
            json!({"type": "text_delta", "contentIndex": 0, "delta": " proxy"}),
            json!({"type": "text_end", "contentIndex": 0, "contentSignature": "sig-1"}),
            json!({"type": "done", "reason": "stop", "usage": proxy_usage()}),
        ],
        std::time::Duration::ZERO,
    );
    let model = proxy_model();
    let provider = pir::sdk::ProxyProvider::new(
        pir::sdk::ProxyEndpoint::new(format!("http://127.0.0.1:{port}"), "local-token"),
        vec![model.clone()],
    )
    .expect("build proxy provider");
    let agent = AgentBuilder::new()
        .proxy(provider, model)
        .cwd(temp.path())
        .system_prompt("You are a proxy stream test agent.")
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build proxy agent");

    let events: Vec<SdkEvent> = agent
        .prompt_stream_with_outcome("proxy question")
        .collect()
        .await;

    // Wire request: route, bearer auth, JSON body with the live context.
    let request = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the loopback server captured the request");
    assert!(
        request.request_line.starts_with("POST /api/stream "),
        "the proxy path is /api/stream: {}",
        request.request_line
    );
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer local-token"),
        "the proxy bearer token is sent"
    );
    assert!(
        request
            .headers
            .get("content-type")
            .is_some_and(|value| value.starts_with("application/json")),
        "the proxy request is JSON: {:?}",
        request.headers.get("content-type")
    );
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("the proxy request body is JSON");
    assert_eq!(body["model"]["id"], "gpt-test");
    assert_eq!(body["model"]["provider"], "proxy");
    assert!(
        body["context"]["messages"]
            .as_array()
            .expect("context.messages is an array")
            .iter()
            .any(|message| message["role"] == "user"
                && message["content"][0]["text"] == "proxy question"),
        "the user prompt is in the proxied context: {}",
        body["context"]
    );
    assert!(body["options"].is_object(), "the request carries options");

    // Client-side event stream and provider usage.
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            SdkEvent::MessageUpdate {
                delta,
                thinking: false,
            } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["Hello", " proxy"], "the SSE deltas are projected");
    let message = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::MessageEnd { message } => Some(message),
            _ => None,
        })
        .expect("the message ends");
    assert_eq!(content_text(&message.content), "Hello proxy");
    assert_eq!(
        message.usage.total_tokens, 5,
        "the terminal usage is decoded"
    );
    let outcome = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::AgentOutcome(outcome) => Some(outcome),
            _ => None,
        })
        .expect("the outcome rides the stream");
    assert_eq!(outcome.message_text().as_deref(), Some("Hello proxy"));
    assert!(matches!(
        events.last(),
        Some(SdkEvent::AgentSettled { aborted: false })
    ));
    let usage = agent.usage_totals();
    assert_eq!(usage.turns, 1);
    assert_eq!(usage.total_tokens, 5);
    agent.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn runtime_fork_wraps_fork_entry_and_selects_the_user_message() {
    let temp = TempDir::new("runtime-fork");
    let cwd = temp.path();
    let agent_dir = cwd.join("agent");
    let session_dir = cwd.join("sessions");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&session_dir).unwrap();

    // Seed a real session with two user turns.
    let seed = agent_builder(
        registry(Arc::new(|_index, _context| {
            assistant(vec![text_block("seed reply")], StopReason::Stop)
        })),
        cwd,
    )
    .agent_dir(&agent_dir)
    .session(SessionMode::Dir(session_dir.clone()))
    .build()
    .await
    .expect("seed agent");
    seed.prompt("first user message")
        .await
        .expect("first seed prompt");
    seed.prompt("second user message")
        .await
        .expect("second seed prompt");
    let source_path = seed.session_path().expect("seed path");
    seed.shutdown().await.expect("seed shutdown");

    // Open the same file through the low-level factory, then wrap it in the
    // runtime (the upstream AgentSessionRuntime call shape).
    let low_registry = registry(Arc::new(|_index, _context| {
        assistant(vec![text_block("unused")], StopReason::Stop)
    }));
    let created = pir::sdk::create_agent_session(CreateAgentSessionOptions {
        cwd: Some(cwd.to_path_buf()),
        agent_dir: Some(agent_dir.clone()),
        model_registry: Some(ModelRegistry::from_registry(Arc::clone(&low_registry))),
        model: Some(mock_model()),
        thinking_level: Some(ThinkingLevel::Off),
        session: Some(SessionManager::open(&source_path).expect("open source session")),
        no_tools: true,
        resource_loader: Some(
            DefaultResourceLoader::new(cwd.to_path_buf(), agent_dir.clone())
                .with_home(cwd.to_path_buf())
                .without_skills()
                .without_prompt_templates(),
        ),
        ..Default::default()
    })
    .await
    .expect("create low-level session");
    let first_user_id = {
        let manager = created.session.session_manager().expect("session manager");
        let guard = manager.lock().await;
        guard
            .entries()
            .into_iter()
            .find_map(|entry| match entry {
                pir::session_manager::FileEntry::Message(message)
                    if message.message.role() == "user" =>
                {
                    Some(message.id.clone())
                }
                _ => None,
            })
            .expect("first user entry")
    };
    let runtime = pir::sdk::AgentSessionRuntime::new(
        Arc::clone(&created.session),
        cwd.to_path_buf(),
        agent_dir,
        Some(session_dir),
    );
    let before = created.session.session_path().expect("before path");
    let outcome = runtime
        .fork(&first_user_id, pir::agent_session::ForkPosition::Before)
        .await
        .expect("runtime fork");
    match outcome {
        pir::agent_session::ForkEntryOutcome::Branched { selected_text } => {
            assert_eq!(
                selected_text.as_deref(),
                Some("first user message"),
                "fork-before returns the selected user text"
            );
        }
        pir::agent_session::ForkEntryOutcome::Cancelled => panic!("fork was cancelled"),
    }
    let after = created.session.session_path().expect("after path");
    assert_ne!(before, after, "fork-from-entry opens a new session file");
    assert_eq!(
        created
            .session
            .session_manager()
            .expect("manager")
            .lock()
            .await
            .parent_session()
            .map(str::to_owned),
        Some(before.to_string_lossy().into_owned()),
        "the forked file records its parent session"
    );
    created.session.shutdown().await;
}

// ---------------------------------------------------------------------------
// Round 42 — hook surface counts/order/payload (normal + abort)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hook_event_surface_counts_order_and_payloads_normal_and_abort() {
    let temp = TempDir::new("hook-surface");
    let log: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = registry(Arc::new(|index, _context| match index {
        0 => assistant(
            vec![tool_call("call-hook", "fast_tool")],
            StopReason::ToolUse,
        ),
        1 => assistant(vec![text_block("normal done")], StopReason::Stop),
        2 => assistant(vec![tool_call("call-hold", "hold")], StopReason::ToolUse),
        _ => assistant(vec![text_block("after abort")], StopReason::Stop),
    }));
    let mut builder = agent_builder(registry, temp.path())
        .no_session()
        .with_tool(Arc::new(FastTool))
        .with_tool(Arc::new(HoldTool {
            name: "hold",
            entered: Arc::clone(&entered),
        }));
    for name in [
        "session_start",
        "input",
        "before_agent_start",
        "tool_call",
        "tool_result",
        "turn_end",
        "agent_settled",
    ] {
        let log = Arc::clone(&log);
        builder = builder.on_event(name, move |payload| {
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((name.to_owned(), payload.clone()));
        });
    }
    let agent = builder.build().await.expect("build hook agent");

    // Normal run: one tool round-trip + final answer.
    let events: Vec<SdkEvent> = agent.prompt_stream("hook prompt").collect().await;
    assert!(matches!(
        events.last(),
        Some(SdkEvent::AgentSettled { aborted: false })
    ));
    let normal = log.lock().unwrap().clone();
    let names: Vec<&str> = normal.iter().map(|(name, _)| name.as_str()).collect();
    let count = |event: &str| names.iter().filter(|name| **name == event).count();
    assert_eq!(count("session_start"), 1, "session_start once: {names:?}");
    assert_eq!(count("input"), 1, "input once: {names:?}");
    assert_eq!(
        count("before_agent_start"),
        1,
        "before_agent_start once: {names:?}"
    );
    assert_eq!(count("tool_call"), 1, "tool_call once: {names:?}");
    assert_eq!(count("tool_result"), 1, "tool_result once: {names:?}");
    assert_eq!(
        count("turn_end"),
        2,
        "turn_end per model round-trip: {names:?}"
    );
    assert_eq!(count("agent_settled"), 1, "agent_settled once: {names:?}");
    let payload = |event: &str| {
        normal
            .iter()
            .find(|(name, _)| name == event)
            .map(|(_, payload)| payload)
            .unwrap_or_else(|| panic!("missing {event}"))
    };
    assert_eq!(payload("session_start")["reason"], "startup");
    assert_eq!(payload("input")["text"], "hook prompt");
    assert_eq!(payload("tool_call")["toolName"], "fast_tool");
    assert_eq!(payload("tool_call")["toolCallId"], "call-hook");
    assert_eq!(payload("tool_result")["toolCallId"], "call-hook");
    assert_eq!(payload("tool_result")["isError"], false);
    assert_eq!(payload("turn_end")["type"], "turn_end");
    assert_eq!(payload("agent_settled")["aborted"], false);
    let pos = |event: &str| {
        names
            .iter()
            .position(|name| *name == event)
            .unwrap_or_else(|| panic!("missing {event}: {names:?}"))
    };
    assert!(pos("session_start") < pos("input"));
    assert!(pos("input") < pos("before_agent_start"));
    assert!(pos("before_agent_start") < pos("tool_call"));
    assert!(pos("tool_call") < pos("tool_result"));
    assert!(pos("tool_result") < pos("turn_end"));
    assert!(pos("turn_end") < pos("agent_settled"));

    // Abort run: the same surface, no duplicate session_start.
    log.lock().unwrap().clear();
    let drain = tokio::spawn({
        let agent = agent.clone();
        async move {
            let mut stream = Box::pin(agent.prompt_stream_with_outcome("abort hook prompt"));
            let mut settled = None;
            let mut outcome = None;
            while let Some(event) = stream.as_mut().next().await {
                match event {
                    SdkEvent::AgentOutcome(value) => outcome = Some(value),
                    SdkEvent::AgentSettled { aborted } => {
                        settled = Some(aborted);
                        break;
                    }
                    _ => {}
                }
            }
            (settled, outcome)
        }
    });
    wait_for_tool_entry(&entered).await;
    agent.abort();
    let (settled, outcome) = drain.await.expect("drain join");
    assert_eq!(settled, Some(true));
    assert!(outcome.expect("aborted outcome").aborted);
    let aborted = log.lock().unwrap().clone();
    let aborted_names: Vec<&str> = aborted.iter().map(|(name, _)| name.as_str()).collect();
    let aborted_count = |event: &str| aborted_names.iter().filter(|name| **name == event).count();
    assert_eq!(
        aborted_count("session_start"),
        0,
        "no duplicate session_start on abort: {aborted_names:?}"
    );
    assert_eq!(aborted_count("input"), 1);
    assert_eq!(aborted_count("before_agent_start"), 1);
    assert_eq!(aborted_count("tool_call"), 1);
    assert_eq!(aborted_count("tool_result"), 1);
    assert_eq!(aborted_count("agent_settled"), 1);
    let payload = |event: &str| {
        aborted
            .iter()
            .find(|(name, _)| name == event)
            .map(|(_, payload)| payload)
            .unwrap_or_else(|| panic!("missing {event}"))
    };
    assert_eq!(payload("tool_call")["toolName"], "hold");
    assert_eq!(payload("tool_result")["toolCallId"], "call-hold");
    assert_eq!(payload("tool_result")["isError"], true);
    assert_eq!(payload("agent_settled")["aborted"], true);
    assert_eq!(
        aborted_names.last().copied(),
        Some("agent_settled"),
        "agent_settled is the last hook event: {aborted_names:?}"
    );
    agent.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn runtime_import_from_jsonl_copies_opens_and_replays_the_session() {
    let temp = TempDir::new("runtime-import");
    let cwd = temp.path();
    let source_dir = cwd.join("source-sessions");
    let target_dir = cwd.join("target-sessions");
    let agent_dir = cwd.join("agent");
    for dir in [&source_dir, &target_dir, &agent_dir] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let seed = agent_builder(
        registry(Arc::new(|_index, _context| {
            assistant(vec![text_block("import reply")], StopReason::Stop)
        })),
        cwd,
    )
    .agent_dir(&agent_dir)
    .session(SessionMode::Dir(source_dir.clone()))
    .build()
    .await
    .expect("seed import agent");
    seed.prompt("import user")
        .await
        .expect("seed import prompt");
    let source_path = seed.session_path().expect("source path");
    seed.shutdown().await.expect("seed shutdown");

    let low_registry = registry(Arc::new(|_index, _context| {
        assistant(vec![text_block("unused")], StopReason::Stop)
    }));
    let created = pir::sdk::create_agent_session(CreateAgentSessionOptions {
        cwd: Some(cwd.to_path_buf()),
        agent_dir: Some(agent_dir.clone()),
        model_registry: Some(ModelRegistry::from_registry(Arc::clone(&low_registry))),
        model: Some(mock_model()),
        thinking_level: Some(ThinkingLevel::Off),
        session: Some(
            SessionManager::create(&cwd.to_string_lossy(), &target_dir)
                .expect("create target session"),
        ),
        no_tools: true,
        resource_loader: Some(
            DefaultResourceLoader::new(cwd.to_path_buf(), agent_dir.clone())
                .with_home(cwd.to_path_buf())
                .without_skills()
                .without_prompt_templates(),
        ),
        ..Default::default()
    })
    .await
    .expect("create target low-level session");
    let runtime = pir::sdk::AgentSessionRuntime::new(
        Arc::clone(&created.session),
        cwd.to_path_buf(),
        agent_dir,
        Some(target_dir.clone()),
    );
    let before = created.session.session_path().expect("before import path");
    let cancelled = runtime
        .import_from_jsonl(&source_path, None)
        .await
        .expect("import session");
    assert!(!cancelled, "no handler cancelled the import");
    let after = created.session.session_path().expect("after import path");
    assert_ne!(after, before, "import swaps the live storage");
    assert_ne!(after, source_path, "the import copies into the session dir");
    assert_eq!(after.parent(), Some(target_dir.as_path()));
    assert!(after.exists(), "the imported copy exists");
    let transcript = created.session.transcript().await;
    assert!(
        transcript.iter().any(|message| {
            matches!(message, Message::User(user)
                if user.content.iter().any(|block| matches!(block, ContentBlock::Text(text) if text.text == "import user")))
        }),
        "the imported transcript is replayed: {transcript:?}"
    );

    // A second import of the same source must not overwrite the first copy.
    let cancelled = runtime
        .import_from_jsonl(&source_path, None)
        .await
        .expect("second import");
    assert!(!cancelled);
    let second = created.session.session_path().expect("second import path");
    assert_ne!(second, after, "a colliding import gets a unique suffix");
    assert!(second.exists());
    created.session.shutdown().await;
}

// ---------------------------------------------------------------------------
// Round 43 — long-conversation RSS soak
// ---------------------------------------------------------------------------

struct SoakTool;

impl AgentTool for SoakTool {
    fn name(&self) -> &'static str {
        "soak_tool"
    }

    fn description(&self) -> &'static str {
        "returns a fixed 2 KiB payload"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object"}))
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("s".repeat(2048)) })
    }
}

/// Long-conversation soak: 400 user turns in ONE process/session, every 20th
/// model call uses a 2 KiB tool result. Samples RSS (tasklist by PID), session
/// file bytes and transcript entries every 25 turns; discards the first 50
/// turns as warmup and checks the post-warmup RSS slope.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual long-conversation RSS soak; run with --ignored --nocapture"]
async fn sdk_rss_long_conversation_soak_has_bounded_slope() {
    use std::time::Instant;

    let pid = std::process::id();
    let temp = TempDir::new("sdk-soak");
    let session_dir = temp.path().join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let registry = registry(Arc::new(|index, _context| {
        if index % 20 == 19 {
            assistant(
                vec![tool_call(&format!("soak-{index}"), "soak_tool")],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text_block("soak reply")], StopReason::Stop)
        }
    }));
    let agent = agent_builder(registry, temp.path())
        .agent_dir(temp.path().join("agent"))
        .session(SessionMode::Dir(session_dir))
        .with_tool(Arc::new(SoakTool))
        .no_compaction()
        .build()
        .await
        .expect("build soak agent");
    let user_text = "u".repeat(512);
    let mut series: Vec<(usize, u64, u64, usize)> = Vec::new(); // turn, rss_kib, file_bytes, entries
    let started = Instant::now();
    for turn in 1..=400usize {
        agent.prompt(&user_text).await.expect("soak prompt");
        if turn % 25 == 0 {
            let rss = tasklist_rss_kib(pid).unwrap_or(0);
            let file_bytes = agent
                .session_path()
                .and_then(|path| std::fs::metadata(&path).ok())
                .map(|meta| meta.len())
                .unwrap_or(0);
            let entries = agent.transcript().await.len();
            println!("SOAK turn={turn} rss_kib={rss} file_bytes={file_bytes} entries={entries}");
            series.push((turn, rss, file_bytes, entries));
        }
    }
    println!(
        "SOAK elapsed_ms={} samples={series:?}",
        started.elapsed().as_millis()
    );

    // Discard the first 50 turns (2 samples) as warmup; fit RSS vs turn.
    let warm: Vec<(f64, f64)> = series
        .iter()
        .filter(|(turn, _, _, _)| *turn > 50)
        .map(|(turn, rss, _, _)| (*turn as f64, *rss as f64))
        .collect();
    assert!(
        warm.len() >= 8,
        "not enough post-warmup samples: {series:?}"
    );
    let n = warm.len() as f64;
    let mean_x = warm.iter().map(|(x, _)| *x).sum::<f64>() / n;
    let mean_y = warm.iter().map(|(_, y)| *y).sum::<f64>() / n;
    let numerator: f64 = warm.iter().map(|(x, y)| (x - mean_x) * (y - mean_y)).sum();
    let denominator: f64 = warm.iter().map(|(x, _)| (x - mean_x).powi(2)).sum();
    let slope_kib_per_turn = numerator / denominator;
    let first_rss = warm.first().map(|(_, y)| *y).unwrap_or(0.0);
    let last_rss = warm.last().map(|(_, y)| *y).unwrap_or(0.0);
    println!(
        "SOAK summary turns=400 slope_kib_per_turn={slope_kib_per_turn:.3} \
         warm_start_kib={first_rss:.0} warm_end_kib={last_rss:.0} \
         growth_mib={:.1}",
        (last_rss - first_rss) / 1024.0
    );
    assert!(
        slope_kib_per_turn < 256.0,
        "long-conversation RSS slope {slope_kib_per_turn:.1} KiB/turn suggests a leak"
    );
    assert!(
        last_rss - first_rss < 256.0 * 1024.0,
        "post-warmup RSS grew more than 256 MiB"
    );
    // The persisted session and entry count must keep tracking the transcript.
    let (last_turn, _, last_bytes, last_entries) = *series.last().unwrap();
    assert_eq!(last_turn, 400);
    assert!(
        last_bytes > 50_000,
        "session file did not grow: {last_bytes}"
    );
    assert!(
        last_entries >= 400,
        "entries did not track turns: {last_entries}"
    );

    // Attribution: reset the transcript and run another 200 turns. If the
    // growth is allocator high-watermark retention, phase 2 grows much less
    // than phase 1; a real per-turn leak grows at the same slope again.
    let phase1_end = last_rss;
    agent.reset().await.expect("reset after phase 1");
    let reset_rss = tasklist_rss_kib(pid).unwrap_or(0) as f64;
    for _turn in 1..=200usize {
        agent.prompt(&user_text).await.expect("phase 2 soak prompt");
    }
    let phase2_end = tasklist_rss_kib(pid).unwrap_or(0) as f64;
    let phase2_growth = (phase2_end - reset_rss) / 1024.0;
    println!(
        "SOAK phase2 reset_kib={reset_rss:.0} end_kib={phase2_end:.0} \
         growth_mib={phase2_growth:.1} (phase1_end_kib={phase1_end:.0})"
    );
    assert!(
        phase2_growth < 128.0,
        "a reset transcript still grew {phase2_growth:.1} MiB in 200 turns"
    );
    agent.shutdown().await.expect("soak shutdown");
}

// ---------------------------------------------------------------------------
// Round 42 — RSS / performance sampling (manual, tasklist by PID)
// ---------------------------------------------------------------------------

/// Manual RSS/perf probe: long multi-tool run, repeated attach over several
/// sessions, and three concurrent agents. Samples this process via `tasklist`
/// by PID; prints baseline/peak/growth and asserts generous bounds so a leak
/// (linear growth) fails instead of passing unnoticed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual RSS/perf probe; run with --ignored --nocapture"]
async fn sdk_rss_long_run_attach_and_concurrency_stay_bounded() {
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    let pid = std::process::id();
    let sample = |tag: &str| -> u64 {
        let kib = tasklist_rss_kib(pid).unwrap_or(0);
        println!("RSS phase={tag} kib={kib} mib={:.1}", kib as f64 / 1024.0);
        kib
    };
    let temp = TempDir::new("sdk-rss-perf");
    let baseline = sample("baseline");

    // 1. Long run: 25 tool loops, each result 75 KiB (context grows ~1.9 MiB).
    let peak = Arc::new(AtomicU64::new(0));
    let peak_writer = Arc::clone(&peak);
    let long_registry = registry(Arc::new(move |index, _context| {
        if let Some(kib) = tasklist_rss_kib(pid) {
            peak_writer.fetch_max(kib, Ordering::SeqCst);
        }
        if index < 25 {
            assistant(
                vec![tool_call(&format!("call-{index}"), "fast_tool")],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text_block("long done")], StopReason::Stop)
        }
    }));
    let long_agent = agent_builder(long_registry, temp.path())
        .no_session()
        .with_tool(Arc::new(LargeFastTool))
        .max_turns(100)
        .build()
        .await
        .expect("build long-run agent");
    let started = Instant::now();
    long_agent
        .prompt("run the long tool sequence")
        .await
        .expect("long prompt");
    let long_ms = started.elapsed().as_millis();
    let after_long = sample("after_long");
    println!(
        "PERF long_run ms={long_ms} peak_kib={} growth_kib={}",
        peak.load(Ordering::SeqCst),
        after_long.saturating_sub(baseline)
    );

    // 2. Repeated attach: three seeded sessions, 20 rounds of attach.
    let seed_dir = temp.path().join("seed-sessions");
    std::fs::create_dir_all(&seed_dir).unwrap();
    let mut paths = Vec::new();
    for index in 0..3 {
        let seed = agent_builder(
            registry(Arc::new(move |_call, _context| {
                assistant(
                    vec![text_block(&format!("seed reply {index}"))],
                    StopReason::Stop,
                )
            })),
            temp.path(),
        )
        .session(SessionMode::Dir(seed_dir.clone()))
        .build()
        .await
        .expect("build seed");
        seed.prompt(&format!("seed question {index}"))
            .await
            .expect("seed prompt");
        paths.push(seed.session_path().expect("seed path"));
        seed.shutdown().await.expect("seed shutdown");
    }
    let target_dir = temp.path().join("target-sessions");
    std::fs::create_dir_all(&target_dir).unwrap();
    let target = agent_builder(
        registry(Arc::new(|_index, _context| {
            assistant(vec![text_block("target reply")], StopReason::Stop)
        })),
        temp.path(),
    )
    .session(SessionMode::Dir(target_dir))
    .build()
    .await
    .expect("build target");
    let attach_baseline = sample("before_attach_loop");
    let attach_started = Instant::now();
    for _round in 0..20 {
        for path in &paths {
            target.attach_session(path).await.expect("attach");
        }
    }
    let after_attach = sample("after_attach_loop");
    println!(
        "PERF attach_loop iterations={} ms={} growth_kib={}",
        60,
        attach_started.elapsed().as_millis(),
        after_attach.saturating_sub(attach_baseline)
    );
    target.shutdown().await.expect("target shutdown");

    // 3. Concurrency: three agents, seven turns each, sampled at every call.
    let concurrent_peak = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for agent_index in 0..3 {
        let peak_writer = Arc::clone(&concurrent_peak);
        let concurrent_registry = registry(Arc::new(move |index, _context| {
            if let Some(kib) = tasklist_rss_kib(pid) {
                peak_writer.fetch_max(kib, Ordering::SeqCst);
            }
            if index < 6 {
                assistant(
                    vec![tool_call(&format!("c{agent_index}-{index}"), "fast_tool")],
                    StopReason::ToolUse,
                )
            } else {
                assistant(
                    vec![text_block(&format!("agent {agent_index} done"))],
                    StopReason::Stop,
                )
            }
        }));
        let agent = agent_builder(concurrent_registry, temp.path())
            .no_session()
            .with_tool(Arc::new(LargeFastTool))
            .max_turns(20)
            .build()
            .await
            .expect("build concurrent agent");
        handles.push(tokio::spawn(async move {
            let started = Instant::now();
            agent
                .prompt("concurrent work")
                .await
                .expect("concurrent prompt");
            (started.elapsed().as_millis(), agent)
        }));
    }
    let mut concurrent_ms = Vec::new();
    let mut agents = Vec::new();
    for handle in handles {
        let (ms, agent) = handle.await.expect("join concurrent agent");
        concurrent_ms.push(ms);
        agents.push(agent);
    }
    let after_concurrent = sample("after_concurrent");
    println!(
        "PERF concurrent agents=3 ms={concurrent_ms:?} peak_kib={} growth_kib={}",
        concurrent_peak.load(Ordering::SeqCst),
        after_concurrent.saturating_sub(baseline)
    );

    // Generous non-leak bounds (the measured numbers are printed above).
    let long_growth_mib = after_long.saturating_sub(baseline) as f64 / 1024.0;
    let attach_growth_mib = after_attach.saturating_sub(attach_baseline) as f64 / 1024.0;
    let concurrent_growth_mib = after_concurrent.saturating_sub(baseline) as f64 / 1024.0;
    println!(
        "PERF summary baseline_kib={baseline} long_growth_mib={long_growth_mib:.1} \
         attach_growth_mib={attach_growth_mib:.1} concurrent_growth_mib={concurrent_growth_mib:.1}"
    );
    assert!(
        long_growth_mib < 256.0,
        "long run retained too much: {long_growth_mib:.1} MiB"
    );
    assert!(
        attach_growth_mib < 128.0,
        "repeated attach retained too much: {attach_growth_mib:.1} MiB"
    );
    assert!(
        concurrent_growth_mib < 512.0,
        "concurrent agents retained too much: {concurrent_growth_mib:.1} MiB"
    );
    for agent in agents {
        agent.shutdown().await.expect("shutdown concurrent agent");
    }
}

#[tokio::test]
async fn proxy_prompt_stream_abort_cancels_a_live_loopback_sse_response() {
    let temp = TempDir::new("proxy-stream-abort");
    let (port, _requests) = spawn_sse_server(
        vec![
            json!({"type": "start"}),
            json!({"type": "text_start", "contentIndex": 0}),
            json!({"type": "text_delta", "contentIndex": 0, "delta": "partial"}),
        ],
        std::time::Duration::from_millis(1500),
    );
    let model = proxy_model();
    let provider = pir::sdk::ProxyProvider::new(
        pir::sdk::ProxyEndpoint::new(format!("http://127.0.0.1:{port}"), "local-token"),
        vec![model.clone()],
    )
    .expect("build proxy provider");
    let agent = AgentBuilder::new()
        .proxy(provider, model)
        .cwd(temp.path())
        .system_prompt("You are a proxy abort test agent.")
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build proxy agent");

    let (delta_sender, delta_receiver) = tokio::sync::oneshot::channel();
    let drain = tokio::spawn({
        let agent = agent.clone();
        async move {
            let mut stream = Box::pin(agent.prompt_stream_with_outcome("abort request"));
            let mut settled = None;
            let mut outcome = None;
            let mut delta_sender = Some(delta_sender);
            while let Some(event) = stream.as_mut().next().await {
                match event {
                    SdkEvent::MessageUpdate { .. } => {
                        if let Some(sender) = delta_sender.take() {
                            let _ = sender.send(());
                        }
                    }
                    SdkEvent::AgentOutcome(value) => outcome = Some(value),
                    SdkEvent::AgentSettled { aborted } => {
                        settled = Some(aborted);
                        break;
                    }
                    _ => {}
                }
            }
            (settled, outcome)
        }
    });
    delta_receiver.await.expect("the first SSE delta arrived");
    agent.abort();
    let (settled, outcome) = tokio::time::timeout(std::time::Duration::from_secs(3), drain)
        .await
        .expect("an aborted proxy stream must settle promptly, not wait for the server")
        .expect("drain join");
    assert_eq!(settled, Some(true), "the stream settles as aborted");
    assert!(
        outcome
            .expect("the aborted run still reports an outcome")
            .aborted,
        "the run is reported as aborted"
    );
    agent.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn live_session_tail_keeps_resident_window_bounded_and_transcript_complete() {
    let cwd = TempDir::new("live-tail");
    let sessions = cwd.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    // R46: also pin that the MODEL CONTEXT stays bounded after a trim — the
    // R45 trim alone was undone by the prompt-start widen, so every request
    // re-materialized the whole transcript and RSS kept climbing.
    let request_message_counts: Arc<std::sync::Mutex<Vec<usize>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = Arc::clone(&request_message_counts);
    let providers = registry(Arc::new(move |_, context| {
        capture.lock().unwrap().push(context.messages.len());
        assistant(vec![text_block("ack")], StopReason::Stop)
    }));
    let agent = AgentBuilder::new()
        .with_registry(Arc::clone(&providers), mock_model())
        .cwd(cwd.path())
        .system_prompt("You are a live-tail retention test agent.")
        .session(SessionMode::Dir(sessions.clone()))
        .live_session_tail(4096, 8)
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build live-tail agent");

    // R47: 200 rounds (400 message entries) so the trim runs repeatedly and
    // the whole-history/resume paths are exercised on a long live session.
    for round in 0..200 {
        agent
            .prompt(format!("turn {round} with enough bytes to matter"))
            .await
            .expect("prompt");
    }
    let counts = request_message_counts.lock().unwrap().clone();
    assert_eq!(counts.len(), 200, "one request per prompt");
    assert!(
        counts[199] <= 10,
        "after the trim the prompt context is the 8-entry tail + the new \
         user message (full history would be 401): {counts:?}"
    );
    assert!(
        agent.is_paged_session().await,
        "the tail budget trims the resident entries between prompts"
    );
    // Entries dropped: the resident page starts late in the file and most of
    // the 400 message entries stayed on disk. (`parsed_entries` here counts
    // the entries parsed INTO the page, i.e. the resident tail.)
    let window = agent.paged_window().await.expect("live-trimmed window");
    assert!(
        window.page_start > 0 && window.page_start * 2 > window.file_bytes,
        "trim must drop the prefix from memory: {window:?}"
    );
    assert!(
        window.parsed_entries <= 8,
        "resident tail must stay within the 8-entry budget: {window:?}"
    );
    let stats = agent.session_stats().await;
    assert_eq!(
        stats.total_messages, 400,
        "session stats stream the skipped prefix and stay whole-session"
    );
    let transcript = agent.transcript().await;
    assert_eq!(
        transcript.len(),
        400,
        "the public transcript widens to the complete history"
    );
    assert!(
        !agent.is_paged_session().await,
        "a whole-history read materializes the session"
    );
    let path = agent.session_path().expect("session path");
    let lines = std::fs::read_to_string(&path)
        .expect("read session")
        .lines()
        .count();
    assert_eq!(lines, 401, "header + 400 message entries stay on disk");
    agent.shutdown().await.expect("shutdown");

    let resumed = AgentBuilder::new()
        .with_registry(
            registry(Arc::new(|_, _| {
                assistant(vec![text_block("ack")], StopReason::Stop)
            })),
            mock_model(),
        )
        .cwd(cwd.path())
        .system_prompt("You are a live-tail retention test agent.")
        .session(SessionMode::Existing(path.clone()))
        .live_session_tail(4096, 8)
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("resume live-tail agent");
    assert_eq!(resumed.transcript().await.len(), 400, "resume replays all");
    resumed
        .prompt("after resume")
        .await
        .expect("resumed prompt");
    assert_eq!(resumed.transcript().await.len(), 402);
    resumed.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn compaction_checkpoint_stays_anchored_to_live_tail_window() {
    // PIR-SDK-024: once the newest compaction entry falls outside the
    // max_entries tail window, the trim must pin it as an anchor (checkpoint
    // summary stays in context) instead of refusing to trim and keeping the
    // whole transcript resident.
    let cwd = TempDir::new("live-tail-compaction-anchor");
    let sessions = cwd.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let requests: Arc<std::sync::Mutex<Vec<(usize, bool)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = Arc::clone(&requests);
    let providers = registry(Arc::new(move |_, context| {
        let is_summary = context
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.starts_with("You are a context summarization assistant"));
        if is_summary {
            return assistant(vec![text_block("SUMMARY-MARKER-024")], StopReason::Stop);
        }
        let has_summary = context
            .messages
            .iter()
            .any(|message| message_text(message).contains("SUMMARY-MARKER-024"));
        capture
            .lock()
            .unwrap()
            .push((context.messages.len(), has_summary));
        assistant(vec![text_block("ack")], StopReason::Stop)
    }));
    let agent = AgentBuilder::new()
        .with_registry(providers, mock_model())
        .cwd(cwd.path())
        .system_prompt("You are a compaction-anchor test agent.")
        .session(SessionMode::Dir(sessions))
        .live_session_tail(4096, 8)
        .compaction(pir::compaction::CompactionConfig {
            enabled: false,
            reserve_tokens: 16_384,
            keep_recent_tokens: 1,
        })
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build live-tail compaction agent");

    for round in 0..6 {
        agent
            .prompt(format!("pre-compact {round}"))
            .await
            .expect("prompt");
    }
    agent.compact().await.expect("manual compaction");
    for round in 0..10 {
        agent
            .prompt(format!("post-compact {round}"))
            .await
            .expect("prompt");
    }

    let requests = requests.lock().unwrap().clone();
    let (last_count, last_has_summary) = *requests.last().expect("requests recorded");
    assert!(
        last_count <= 12,
        "the prompt context must stay inside the 8-entry tail + checkpoint + \
         the new user message, not grow to the whole transcript: {last_count}"
    );
    assert!(
        last_has_summary,
        "the anchored compaction checkpoint summary must stay in the prompt context"
    );
    assert!(
        agent.is_paged_session().await,
        "the session stays bounded after the checkpoint leaves the entry window"
    );
    let window = agent.paged_window().await.expect("live-trimmed window");
    assert!(window.parsed_entries <= 8, "tail entry budget: {window:?}");
    let stats = agent.session_stats().await;
    assert_eq!(stats.total_messages, 32, "stats see the whole session");

    let transcript = agent.transcript().await;
    assert!(
        transcript
            .iter()
            .any(|message| message_text(message).contains("SUMMARY-MARKER-024")),
        "the public transcript widens and keeps the latest checkpoint"
    );
    let path = agent.session_path().expect("session path");
    assert_eq!(
        std::fs::read_to_string(&path)
            .expect("read session")
            .lines()
            .count(),
        34,
        "header + 12 pre-compact + 1 compaction + 20 post-compact entries"
    );
    agent.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn transcript_of_resumed_large_paged_session_is_complete_on_first_read() {
    // PIR-SDK-025: the first whole-history read after resuming a >1024-entry
    // paged session must widen, not return the default resident page.
    let cwd = TempDir::new("paged-resume-first-read");
    let sessions = cwd.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let agent = AgentBuilder::new()
        .with_registry(
            registry(Arc::new(|_, _| {
                assistant(vec![text_block("ack")], StopReason::Stop)
            })),
            mock_model(),
        )
        .cwd(cwd.path())
        .system_prompt("You are a paged resume test agent.")
        .session(SessionMode::Dir(sessions))
        .live_session_tail(4096, 8)
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build paged agent");
    for round in 0..520 {
        agent
            .prompt(format!("round {round}"))
            .await
            .expect("prompt");
    }
    let path = agent.session_path().expect("session path");
    assert_eq!(
        std::fs::read_to_string(&path)
            .expect("read session")
            .lines()
            .count(),
        1041,
        "header + 1040 message entries (above the 1024-entry page)"
    );
    agent.shutdown().await.expect("shutdown");

    let resume = |cwd: &Path| {
        AgentBuilder::new()
            .with_registry(
                registry(Arc::new(|_, _| {
                    assistant(vec![text_block("ack")], StopReason::Stop)
                })),
                mock_model(),
            )
            .cwd(cwd)
            .system_prompt("You are a paged resume test agent.")
            .session(SessionMode::Existing(path.clone()))
            .live_session_tail(4096, 8)
            .no_compaction()
            .thinking_level(ThinkingLevel::Off)
    };
    let resumed = resume(cwd.path())
        .build()
        .await
        .expect("resume paged agent");
    assert!(resumed.is_paged_session().await, "resume opens tail-first");
    assert_eq!(
        resumed.transcript().await.len(),
        1040,
        "the FIRST transcript() widens to the complete history"
    );
    resumed.shutdown().await.expect("shutdown");

    let resumed = resume(cwd.path()).build().await.expect("resume again");
    assert_eq!(
        resumed.transcript_json().await.len(),
        1040,
        "transcript_json widens too"
    );
    resumed.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// R54: `build_session_messages` is the exact messages-only twin of
// `build_session_projection().messages` (same leaf path, retention, context
// edits and newest-compaction rule; no per-entry provenance metadata).
// ---------------------------------------------------------------------------

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1,
    })
}

fn assistant_text(text: &str) -> Message {
    Message::Assistant(assistant(vec![text_block(text)], StopReason::Stop))
}

fn assert_messages_projection_equivalent(session: &SessionManager, scenario: &str) {
    let projection = session.build_session_projection().expect("full projection");
    let messages = session.build_session_messages();
    assert_eq!(
        messages, projection.messages,
        "{scenario}: messages-only aggregate diverged from the full projection"
    );
    // The generic borrowed-slice entry point (`&[FileEntry]`, used by export
    // paths and tests) must agree with the manager's `Arc<FileEntry>` walk.
    let entries = session.entries();
    let from_slice =
        pir::session_manager::projection::build_session_messages(&entries, session.leaf_id());
    assert_eq!(
        from_slice, messages,
        "{scenario}: &[FileEntry] walk diverged from the manager walk"
    );
}

#[test]
fn messages_only_projection_matches_full_projection_for_branched_history() {
    let mut session = SessionManager::in_memory("C:/repo", None).unwrap();
    let root = session.append_message(user("root question")).unwrap();
    session
        .append_message(assistant_text("root answer"))
        .unwrap();
    session.append_message(user("discarded question")).unwrap();
    session
        .append_message(assistant_text("discarded answer"))
        .unwrap();
    // Branch back to the root: the active leaf path drops the tail above.
    session.branch(&root).unwrap();
    session
        .branch_with_summary(
            Some(&root),
            "summary of the dropped branch",
            None,
            None,
            None,
        )
        .unwrap();
    session.append_message(user("active follow-up")).unwrap();
    session
        .append_custom_message_entry(
            "test.note",
            json!([{"type": "text", "text": "custom note"}]),
            true,
            None,
        )
        .unwrap();

    assert_messages_projection_equivalent(&session, "branched");
    let texts: Vec<String> = session
        .build_session_messages()
        .iter()
        .map(message_text)
        .collect();
    assert_eq!(texts.len(), 4, "{texts:?}");
    assert_eq!(texts[0], "root question");
    assert!(
        texts[1].contains("summary of the dropped branch"),
        "{texts:?}"
    );
    assert_eq!(texts[2], "active follow-up");
    assert_eq!(texts[3], "custom note");
}

#[test]
fn messages_only_projection_matches_full_projection_with_context_edits() {
    let mut session = SessionManager::in_memory("C:/repo", None).unwrap();
    let q1 = session.append_message(user("question one")).unwrap();
    let a1 = session
        .append_message(assistant_text("answer one"))
        .unwrap();
    let q2 = session.append_message(user("question two")).unwrap();
    let a2 = session
        .append_message(assistant_text("answer two"))
        .unwrap();
    // A replacement on an assistant message (string -> text block), a
    // replacement on a user message (block array) and an omission.
    session
        .append_context_edit(&a1, Some(json!("rewritten answer one")))
        .unwrap();
    session
        .append_context_edit(
            &q2,
            Some(json!([{"type": "text", "text": "rewritten question two"}])),
        )
        .unwrap();
    session.append_context_edit(&a2, None).unwrap();
    session
        .append_context_edit(&q1, Some(json!([{"type": "text", "text": "question one"}])))
        .unwrap();

    assert_messages_projection_equivalent(&session, "context-edits");
    let texts: Vec<String> = session
        .build_session_messages()
        .iter()
        .map(message_text)
        .collect();
    assert_eq!(
        texts,
        vec![
            "question one",
            "rewritten answer one",
            "rewritten question two",
        ],
        "latest edit wins; a null replacement omits the target"
    );
}

#[test]
fn messages_only_projection_matches_full_projection_across_compaction_shapes() {
    let cwd = TempDir::new("messages-equivalence-compaction");
    let sessions = cwd.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut session = SessionManager::create("C:/repo", &sessions).unwrap();
    session.append_message(user("q1")).unwrap();
    session.append_message(assistant_text("a1")).unwrap();
    // A system contribution on the path is replayed by the compaction entry
    // (two messages: stored system replay + summary).
    session
        .append_message(Message::System(UserMessage {
            role: UserRole::System,
            content: vec![text_block("stored system")],
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp: 1,
        }))
        .unwrap();
    let q2 = session.append_message(user("q2")).unwrap();
    session.append_message(assistant_text("a2")).unwrap();
    session
        .append_compaction("first summary", Some(&q2), 100, None)
        .unwrap();
    session.append_message(user("q3")).unwrap();

    assert_messages_projection_equivalent(&session, "compaction-retained");
    let projection = session.build_session_projection().unwrap();
    assert!(
        projection.messages.len() >= 4,
        "summary + stored system replay + retained q2/a2 + q3: {:?}",
        projection.messages
    );

    // A self-retaining compaction replaces the first; the older compaction
    // (now inside the retained range) must contribute nothing.
    session
        .append_compaction("second summary", None, 50, None)
        .unwrap();
    session.append_message(user("q4")).unwrap();
    assert_messages_projection_equivalent(&session, "compaction-self-retaining");

    // The persisted `Arc<FileEntry>` walk (reopen) must match too.
    let path = session.path().to_path_buf();
    drop(session);
    let reopened = SessionManager::open(&path).expect("reopen");
    assert_messages_projection_equivalent(&reopened, "compaction-reopened");
}

#[test]
fn messages_only_projection_matches_full_projection_on_paged_manager() {
    let cwd = TempDir::new("messages-equivalence-paged");
    let sessions = cwd.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut session = SessionManager::create("C:/repo", &sessions).unwrap();
    for index in 0..40 {
        session
            .append_message(user(&format!("message {index}")))
            .unwrap();
    }
    let path = session.path().to_path_buf();
    drop(session);

    // A paged manager projects its resident tail window; both builders must
    // walk that same window (whole-history readers widen first, PIR-SDK-025).
    let paged = SessionManager::open_paged(&path, 2048, 8).expect("paged open");
    assert!(paged.is_paged());
    assert_messages_projection_equivalent(&paged, "paged");
    assert!(
        paged.build_session_messages().len() <= 8,
        "the paged manager projects only its resident entry window"
    );
}
