//! RSS-B: incremental prompt projection cache.
//!
//! A steady conversation must pay exactly ONE full `SessionProjection` build:
//! every later prompt extends the previous projection with the entries
//! appended since its leaf. Context edits / compactions / branch resets must
//! fall back to a full build, and tool results / multimodal blocks must survive
//! the reuse path unchanged.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::agent_loop::AgentEvent;
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::session_manager::{FileEntry, SessionManager};

const TEST_ROOT: &str = env!("CARGO_TARGET_TMPDIR");

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path =
            std::path::Path::new(TEST_ROOT).join(format!("rss-b-{name}-{}", std::process::id()));
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

fn image(data: &str) -> ContentBlock {
    ContentBlock::Image(ImageContent {
        data: data.into(),
        mime_type: "image/png".into(),
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

type RequestLog = Arc<Mutex<Vec<Vec<Message>>>>;

struct FakeProvider {
    calls: AtomicUsize,
    requests: RequestLog,
    respond: Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>,
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
    fn api_key_auth(&self) -> Vec<pir::ai::auth::ApiKeyAuth> {
        Vec::new()
    }
    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(context.messages.clone());
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn registry(
    requests: RequestLog,
    respond: impl Fn(usize, &Context) -> AssistantMessage + Send + Sync + 'static,
) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        requests,
        respond: Arc::new(respond),
    }));
    Arc::new(registry)
}

fn agent(
    session: SessionManager,
    registry: Arc<ProviderRegistry>,
    tools: ToolRegistry,
) -> AgentSession {
    AgentSession::new_with_plugins(
        AgentSessionConfig {
            performance: Default::default(),
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session: Some(session),
            compaction: None,
            max_turns: 6,
            system_prompt: None,
            cwd: Some(".".into()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        Vec::new(),
    )
}

fn noop_events() -> pir::agent_session::EventHandler {
    Arc::new(|_event: AgentEvent| {})
}

fn message_text(message: &Message) -> String {
    let content = match message {
        Message::User(user) | Message::System(user) | Message::Developer(user) => &user.content,
        Message::Assistant(assistant) => &assistant.content,
        Message::ToolResult(result) => &result.content,
    };
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn request_texts(requests: &RequestLog, index: usize) -> Vec<String> {
    requests.lock().unwrap()[index]
        .iter()
        .map(message_text)
        .collect()
}

fn entry_id_matching(session: &SessionManager, predicate: impl Fn(&FileEntry) -> bool) -> String {
    session
        .resident_entries()
        .iter()
        .find(|entry| predicate(entry))
        .and_then(|entry| entry.id())
        .expect("matching entry")
        .to_owned()
}

/// N consecutive prompts: ONE full projection, N-1 incremental extensions;
/// the appended user/assistant messages appear in order and unchanged.
#[tokio::test]
async fn consecutive_prompts_build_once_and_extend_in_order() {
    let temp = TempDir::new("reuse");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let registry = registry(Arc::clone(&requests), |_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    });
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(agent(session, registry, ToolRegistry::new()));

    for prompt in ["one", "two", "three"] {
        agent.prompt(prompt, noop_events()).await.expect("prompt");
    }

    let builds = agent
        .with_session_manager(|manager| manager.projection_build_count())
        .await;
    assert_eq!(
        builds,
        Some(1),
        "three prompts must materialize the transcript exactly once"
    );
    assert_eq!(
        agent.projection_reuse_count(),
        2,
        "prompt 2 and 3 must both reuse the incremental snapshot"
    );

    assert_eq!(
        request_texts(&requests, 1),
        vec!["one", "ok", "two"],
        "second request carries the first turn followed by the new user message"
    );
    assert_eq!(
        request_texts(&requests, 2),
        vec!["one", "ok", "two", "ok", "three"],
        "third request carries the full linear history in order"
    );
}

/// A usage record is a state entry: it must neither force a rebuild nor add a
/// message to the request context.
#[tokio::test]
async fn appended_usage_entry_keeps_the_incremental_path() {
    let temp = TempDir::new("usage");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let registry = registry(Arc::clone(&requests), |_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    });
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(agent(session, registry, ToolRegistry::new()));

    agent.prompt("first", noop_events()).await.expect("prompt");
    agent
        .with_session_manager(|manager| {
            manager
                .append_usage("turn", "fake", "fake-model", Usage::default(), None)
                .expect("append usage")
        })
        .await
        .expect("session");

    agent.prompt("second", noop_events()).await.expect("prompt");
    assert_eq!(
        agent
            .with_session_manager(|manager| manager.projection_build_count())
            .await,
        Some(1),
        "a usage state entry must not invalidate the projection snapshot"
    );
    assert_eq!(agent.projection_reuse_count(), 1);
    assert_eq!(
        request_texts(&requests, 1),
        vec!["first", "ok", "second"],
        "the usage entry contributes no message"
    );
}

/// A context edit changes the meaning of an already-projected message: the
/// next prompt must rebuild the whole projection (and see the edited text).
#[tokio::test]
async fn context_edit_falls_back_to_full_projection() {
    let temp = TempDir::new("edit");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let registry = registry(Arc::clone(&requests), |_index, _context| {
        assistant(vec![text("original")], StopReason::Stop)
    });
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(agent(session, registry, ToolRegistry::new()));

    agent.prompt("first", noop_events()).await.expect("prompt");
    let before = agent
        .with_session_manager(|manager| manager.projection_build_count())
        .await
        .expect("session");

    let target = agent
        .with_session_manager(|manager| {
            entry_id_matching(manager, |entry| {
                matches!(
                    entry,
                    FileEntry::Message(inner)
                        if matches!(inner.message.role(), "assistant")
                )
            })
        })
        .await
        .expect("session");
    agent
        .with_session_manager(|manager| {
            manager
                .append_context_edit(&target, Some(serde_json::json!("edited")))
                .expect("append edit")
        })
        .await
        .expect("session");

    agent.prompt("second", noop_events()).await.expect("prompt");
    let after = agent
        .with_session_manager(|manager| manager.projection_build_count())
        .await
        .expect("session");
    assert_eq!(
        after,
        before + 1,
        "a context edit must force exactly one full rebuild"
    );
    assert_eq!(
        agent.projection_reuse_count(),
        0,
        "no prompt may extend past a context edit"
    );
    assert_eq!(
        request_texts(&requests, 1),
        vec!["first", "edited", "second"],
        "the rebuilt context applies the edit"
    );
}

/// A compaction changes the retained range: the next prompt must rebuild.
#[tokio::test]
async fn compaction_falls_back_to_full_projection() {
    let temp = TempDir::new("compaction");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let registry = registry(Arc::clone(&requests), |_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    });
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(agent(session, registry, ToolRegistry::new()));

    agent.prompt("first", noop_events()).await.expect("prompt");
    let before = agent
        .with_session_manager(|manager| manager.projection_build_count())
        .await
        .expect("session");
    let first_kept = agent
        .with_session_manager(|manager| {
            entry_id_matching(manager, |entry| matches!(entry, FileEntry::Message(_)))
        })
        .await
        .expect("session");
    agent
        .with_session_manager(|manager| {
            manager
                .append_compaction("compacted summary", Some(&first_kept), 100, None)
                .expect("append compaction")
        })
        .await
        .expect("session");

    agent.prompt("second", noop_events()).await.expect("prompt");
    let after = agent
        .with_session_manager(|manager| manager.projection_build_count())
        .await
        .expect("session");
    assert_eq!(
        after,
        before + 1,
        "a compaction entry must force exactly one full rebuild"
    );
    assert_eq!(
        agent.projection_reuse_count(),
        0,
        "no prompt may extend across a compaction"
    );
    let second_request = request_texts(&requests, 1);
    assert!(
        second_request
            .iter()
            .any(|text| text.contains("compacted summary")),
        "the rebuilt context carries the summary: {second_request:?}"
    );
}

/// A leaf reset (rewrite/branch) makes the cached leaf unreachable: rebuild.
#[tokio::test]
async fn branch_reset_falls_back_to_full_projection() {
    let temp = TempDir::new("branch");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let registry = registry(Arc::clone(&requests), |_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    });
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(agent(session, registry, ToolRegistry::new()));

    agent.prompt("first", noop_events()).await.expect("prompt");
    let before = agent
        .with_session_manager(|manager| manager.projection_build_count())
        .await
        .expect("session");
    agent
        .with_session_manager(|manager| manager.reset_leaf())
        .await
        .expect("session");

    agent
        .prompt("new root", noop_events())
        .await
        .expect("prompt");
    let after = agent
        .with_session_manager(|manager| manager.projection_build_count())
        .await
        .expect("session");
    assert_eq!(
        after,
        before + 1,
        "an unreachable cached leaf must force exactly one full rebuild"
    );
    assert_eq!(
        request_texts(&requests, 1),
        vec!["new root"],
        "the new branch starts without the old transcript"
    );
}

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

/// Tool results (assistant toolCall -> toolResult -> assistant) and image
/// blocks must be carried through the incremental path unchanged.
#[tokio::test]
async fn tool_result_and_multimodal_survive_reuse() {
    let temp = TempDir::new("tool-image");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let registry = registry(Arc::clone(&requests), |index, _context| match index {
        0 => assistant(vec![tool_call("call-1", "echo")], StopReason::ToolUse),
        1 => assistant(vec![text("done")], StopReason::Stop),
        _ => assistant(vec![text("done again")], StopReason::Stop),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let agent = Arc::new(agent(session, registry, tools));

    agent
        .prompt_with_images("with image", vec![image("iVBORw0KGgo=")], noop_events())
        .await
        .expect("first prompt");
    agent
        .prompt("second", noop_events())
        .await
        .expect("second prompt");

    assert_eq!(
        agent
            .with_session_manager(|manager| manager.projection_build_count())
            .await,
        Some(1),
        "the tool turn must not force a second full projection"
    );
    assert_eq!(agent.projection_reuse_count(), 1);

    // Request index 2 is the second prompt's first request: it must contain
    // the whole replayed first turn — user image, assistant tool call, tool
    // result — plus the new user message.
    let messages = requests.lock().unwrap()[2].clone();
    assert!(
        messages.iter().any(|message| matches!(
            message,
            Message::User(user) if user.content.iter().any(|block| matches!(block, ContentBlock::Image(_)))
        )),
        "the image block must survive the incremental projection: {messages:?}"
    );
    assert!(
        messages.iter().any(|message| matches!(
            message,
            Message::Assistant(assistant) if assistant.content.iter().any(|block| matches!(
                block,
                ContentBlock::ToolCall(call) if call.id == "call-1"
            ))
        )),
        "the intermediate assistant tool call must survive: {messages:?}"
    );
    assert!(
        messages.iter().any(|message| matches!(
            message,
            Message::ToolResult(result) if result.tool_call_id == "call-1"
        )),
        "the tool result must survive: {messages:?}"
    );
    assert_eq!(
        message_text(messages.last().expect("last message")),
        "second",
        "the new user message is appended after the replayed history"
    );
}
