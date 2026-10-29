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

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, ErrorReason, ImageContent,
    Message, Model, ModelCost, ModelCostRates, StopReason, StreamOptions, TerminalReason,
    TextContent, ThinkingLevel, Usage,
};
use pir::sdk::{AgentBuilder, SdkEvent, SessionMode};
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
