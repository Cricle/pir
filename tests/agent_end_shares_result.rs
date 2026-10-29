//! P12: `AgentEvent::AgentEnd` shares ONE `LoopResult` allocation with the
//! run's return value instead of deep-cloning terminal/tool results/new
//! messages at emit time.
//!
//! Two legs:
//! * the loop's `AgentEnd` result and `run_with_options_shared`'s return are
//!   the same allocation (`Arc::ptr_eq`) and carry the same tool results;
//! * the SDK mapping (`AgentEvent::AgentEnd` -> `LoopOutcome` ->
//!   `AgentOutcome`) still yields the run's tool results end to end.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::agent_loop::{
    run_with_options_shared, AgentEvent, EventHandler, LoopDeps, LoopOptions, LoopResult, StreamFn,
};
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, TextContent, ThinkingLevel, ToolCall,
};
use pir::sdk::AgentBuilder;

fn text(content: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: content.into(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake-api".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

fn tool_call_message() -> AssistantMessage {
    assistant(
        vec![ContentBlock::ToolCall(ToolCall {
            id: "call-1".into(),
            name: "echo".into(),
            arguments: BTreeMap::new(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        StopReason::ToolUse,
    )
}

/// A tool that answers with a fixed text.
struct EchoTool;

impl AgentTool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "answers with a fixed text"
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("echoed") })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn agent_end_result_is_the_returned_allocation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let stream: StreamFn = Box::new({
        let calls = Arc::clone(&calls);
        move |_request| {
            let message = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                tool_call_message()
            } else {
                assistant(vec![text("done")], StopReason::Stop)
            };
            Box::pin(async move { message })
        }
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));

    let emitted: Arc<Mutex<Option<Arc<LoopResult>>>> = Arc::default();
    let slot = Arc::clone(&emitted);
    let handler: EventHandler = Arc::new(move |event: AgentEvent| {
        if let AgentEvent::AgentEnd { result } = event {
            *slot.lock().unwrap() = Some(result);
        }
    });

    let returned = run_with_options_shared(
        LoopDeps {
            stream,
            tools: Arc::new(tools),
            max_turns: 4,
            ..LoopDeps::default()
        },
        Context::default(),
        None,
        LoopOptions {
            events: Some(handler),
            ..LoopOptions::default()
        },
    )
    .await;

    let emitted = emitted
        .lock()
        .unwrap()
        .take()
        .expect("the run emitted AgentEnd");
    assert!(
        Arc::ptr_eq(&emitted, &returned),
        "AgentEnd.result and the run's return value must be ONE allocation"
    );
    // The shared result still carries the run's tool results, unchanged.
    assert_eq!(emitted.tool_results.len(), 1);
    assert_eq!(returned.tool_results.len(), 1);
    assert_eq!(returned.tool_results[0].tool_call_id, "call-1");
    assert_eq!(returned.tool_results[0].tool_name, "echo");
}

// ---------------------------------------------------------------------------
// SDK leg: the AgentEnd consumer still builds the same outcome.
// ---------------------------------------------------------------------------

fn mock_model() -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock model".into(),
        api: "mock-api".into(),
        provider: "mock".into(),
        base_url: "http://127.0.0.1:1".into(),
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
        context_window: 4_096,
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

/// Answers the first request with an `echo` call and the second with text.
struct MockProvider {
    calls: AtomicUsize,
}

impl Provider for MockProvider {
    fn id(&self) -> &str {
        "mock"
    }

    fn name(&self) -> &str {
        "Mock provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![mock_model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("mock key", vec!["MOCK_TEST_KEY".into()])]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let message = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            tool_call_message()
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_outcome_still_carries_the_run_tool_results() {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(MockProvider {
        calls: AtomicUsize::new(0),
    }));

    let agent = AgentBuilder::new()
        .with_registry(Arc::new(registry), mock_model())
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .tool(
            "echo",
            "answers with a fixed text",
            serde_json::json!({"type": "object"}),
            |_args| async { ToolResult::ok("echoed") },
        )
        .build()
        .await
        .expect("build the SDK agent");

    let outcome = agent.prompt("use echo").await.expect("prompt");
    assert_eq!(outcome.tool_results.len(), 1, "the SDK outcome's results");
    assert_eq!(outcome.tool_results[0].tool_call_id, "call-1");
    assert_eq!(outcome.tool_results[0].tool_name, "echo");

    agent.shutdown().await.expect("shutdown");
}
