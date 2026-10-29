//! Shared-transcript pins for the provider boundary and lifecycle events.
//!
//! The agent loop keeps its transcript behind ONE `Arc<Context>` for the whole
//! run. These tests pin the two places the former implementation materialized
//! a transcript-sized owned copy:
//!
//! * the provider boundary (`ProviderRequest.context` was turned back into an
//!   owned `Context` on every request), and
//! * the `AgentStart` / `TurnStart` / `AgentEnd` event payloads and
//!   `LoopResult.context`.
//!
//! Both now hand the SAME allocation through (refcount bumps), which is what
//! the pointer/strong-count assertions below check without needing an
//! allocator.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::agent_loop::{
    run_with_options, AgentEvent, EventHandler, LoopDeps, LoopOptions, PrepareRequestContext,
    PrepareRequestUpdate,
};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, Context, Message, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, UserMessage, UserRole,
};

fn test_model() -> Model {
    Model {
        id: "shared-context-model".into(),
        name: "Shared context model".into(),
        api: "test-api".into(),
        provider: "shared-context".into(),
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

fn terminal_stream() -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        AssistantMessage {
            content: vec![pir::ai::types::ContentBlock::Text(
                pir::ai::types::TextContent {
                    text: "done".into(),
                    text_signature: None,
                },
            )],
            stop_reason: StopReason::Stop,
            provider: "shared-context".into(),
            api: "test-api".into(),
            model: "shared-context-model".into(),
            ..AssistantMessage::default()
        },
    ));
    stream
}

/// Records the allocation the provider was handed and which entry point
/// (`stream` vs `stream_shared`) the registry used.
#[derive(Default)]
struct SharedContextProvider {
    shared_ptrs: Mutex<Vec<usize>>,
    owned_calls: AtomicUsize,
}

impl Provider for SharedContextProvider {
    fn id(&self) -> &str {
        "shared-context"
    }

    fn name(&self) -> &str {
        "Shared context provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![test_model()]
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
        self.owned_calls.fetch_add(1, Ordering::SeqCst);
        terminal_stream()
    }

    fn stream_shared(
        &self,
        _model: &Model,
        context: Arc<Context>,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.shared_ptrs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::as_ptr(&context) as usize);
        terminal_stream()
    }
}

fn user_context(text: &str) -> Context {
    Context {
        system_prompt: Some("system".into()),
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![pir::ai::types::ContentBlock::Text(
                pir::ai::types::TextContent {
                    text: text.into(),
                    text_signature: None,
                },
            )],
            tools_added: None,
            tools_removed: None,
            timestamp: 0,
            sections: None,
        })],
        tools: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn provider_and_lifecycle_events_reuse_the_loops_transcript_allocation() {
    let provider = Arc::new(SharedContextProvider::default());
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::clone(&provider) as Arc<dyn Provider>);
    let registry = Arc::new(registry);

    // `prepare_request` is handed the loop's own handle: with no replacement
    // update it must be the same allocation the provider receives.
    let prepare_ptrs: Arc<Mutex<Vec<usize>>> = Arc::default();
    let hook_ptrs = Arc::clone(&prepare_ptrs);
    let prepare_request: pir::agent::agent_loop::PrepareRequestHook =
        Arc::new(move |context: PrepareRequestContext| {
            hook_ptrs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Arc::as_ptr(&context.context) as usize);
            Box::pin(async { None::<PrepareRequestUpdate> })
        });

    // Observed event payloads: (kind, allocation, strong count while the
    // handler owns its handle). A deep copy would be a NEW allocation with
    // count 1; the loop's shared handle has count >= 2 here.
    type Observation = (&'static str, usize, usize);
    let observations: Arc<Mutex<Vec<Observation>>> = Arc::default();
    let handler_observations = Arc::clone(&observations);
    let handler: EventHandler = Arc::new(move |event: AgentEvent| {
        let observation = match event {
            AgentEvent::AgentStart { context } => Some((
                "agent_start",
                Arc::as_ptr(&context) as usize,
                Arc::strong_count(&context),
            )),
            AgentEvent::TurnStart { context, .. } => Some((
                "turn_start",
                Arc::as_ptr(&context) as usize,
                Arc::strong_count(&context),
            )),
            AgentEvent::AgentEnd { result } => Some((
                "agent_end",
                Arc::as_ptr(&result.context) as usize,
                Arc::strong_count(&result.context),
            )),
            _ => None,
        };
        if let Some(observation) = observation {
            handler_observations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(observation);
        }
    });

    let deps = LoopDeps {
        prepare_request: Some(prepare_request),
        max_turns: 1,
        ..LoopDeps::default()
    }
    .with_provider_unchecked(
        Arc::clone(&registry),
        test_model(),
        StreamOptions::default(),
    );

    let options = LoopOptions {
        events: Some(handler),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, user_context("hello"), None, options).await;

    let loop_ptr = Arc::as_ptr(&result.context) as usize;

    // The provider was handed the loop's allocation through the shared entry
    // point; the owned `stream` entry point was never used.
    let provider_ptrs = provider
        .shared_ptrs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(provider_ptrs.len(), 1, "one provider request");
    assert_eq!(
        provider_ptrs[0], loop_ptr,
        "the provider must stream from the loop's shared allocation"
    );
    assert_eq!(
        provider.owned_calls.load(Ordering::SeqCst),
        0,
        "the registry must use Provider::stream_shared"
    );
    drop(provider_ptrs);

    // `prepare_request` saw the same allocation it later streamed from.
    let hook_ptrs = prepare_ptrs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(hook_ptrs.len(), 1);
    assert_eq!(hook_ptrs[0], loop_ptr);
    drop(hook_ptrs);

    // Both lifecycle events and the final result carry that same handle, and
    // the handler observed it while the loop still held a reference (count
    // >= 2) — i.e. a refcount bump, never a copy.
    let observations = observations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut kinds: Vec<&str> = observations.iter().map(|(kind, _, _)| *kind).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, vec!["agent_end", "agent_start", "turn_start"]);
    for (kind, ptr, strong_count) in observations.iter() {
        assert_eq!(*ptr, loop_ptr, "{kind} must carry the shared transcript");
        // Pointer identity is the no-copy proof. The lifecycle events are
        // emitted while the loop still holds its own handle (count >= 2);
        // `agent_end` carries the final `Arc<LoopResult>`, into which the loop
        // has MOVED its transcript handle, so the event can be the only holder
        // (count == 1) — that is sharing, not a copy.
        if *kind != "agent_end" {
            assert!(
                *strong_count >= 2,
                "{kind} carries a refcount bump (count={strong_count}), not a deep copy"
            );
        }
    }
}
