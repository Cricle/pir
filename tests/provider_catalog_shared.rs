//! Catalog/wrapper shared-transcript pins.
//!
//! The agent loop keeps ONE `Arc<Context>` for a run and hands it to
//! `Provider::stream_shared`. The built-in catalog path used to defeat that:
//! `CatalogProviderEntry` only implemented `Provider::stream`, so the trait
//! default materialized a transcript-sized owned copy on every request even
//! for adapters (Anthropic/OpenAI/Mistral/Bedrock/...) that stream from the
//! shared allocation. `OAuthFlowOverlay` had the same gap.
//!
//! These tests pin the passthrough: a catalog entry wrapping a recording
//! adapter sees the loop's exact allocation, and the OAuth overlay forwards
//! the same handle instead of falling back to the owned entry point.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::agent_loop::{run_with_options, LoopDeps, LoopOptions};
use pir::ai::auth::{
    ApiKeyAuth, AuthInteraction, LoginOptions, OAuthCredential, OAuthFlow, OAuthFlowBox,
};
use pir::ai::error::{AiError, AiResult};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderBox, ProviderRegistry};
use pir::ai::providers::js_stream::with_oauth_flow;
use pir::ai::providers::CatalogProviderEntry;
use pir::ai::types::{
    AssistantMessage, Context, Message, Model, ModelAuth, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, UserMessage, UserRole,
};

fn test_model() -> Model {
    Model {
        id: "catalog-shared-model".into(),
        name: "Catalog shared model".into(),
        api: "test-api".into(),
        provider: "catalog-shared".into(),
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
            stop_reason: StopReason::Stop,
            provider: "catalog-shared".into(),
            api: "test-api".into(),
            model: "catalog-shared-model".into(),
            ..AssistantMessage::default()
        },
    ));
    stream
}

/// Records which provider entry point was used and the `Arc<Context>`
/// allocation handed to the shared one.
#[derive(Default)]
struct RecordingAdapter {
    shared_ptrs: Mutex<Vec<usize>>,
    owned_calls: AtomicUsize,
}

impl RecordingAdapter {
    fn shared_ptrs(&self) -> Vec<usize> {
        self.shared_ptrs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn owned_calls(&self) -> usize {
        self.owned_calls.load(Ordering::SeqCst)
    }
}

impl Provider for RecordingAdapter {
    fn id(&self) -> &str {
        "catalog-shared-adapter"
    }

    fn name(&self) -> &str {
        "Catalog shared adapter"
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

fn catalog_with(adapter: Arc<RecordingAdapter>) -> ProviderBox {
    let adapters = std::collections::BTreeMap::from([(
        "test-api".to_string(),
        Some(Arc::clone(&adapter) as ProviderBox),
    )]);
    CatalogProviderEntry::from_adapters(
        "catalog-shared",
        "Catalog shared",
        adapters,
        vec![test_model()],
    )
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

/// A real agent-loop request routed through `ProviderRegistry::stream_unchecked`
/// -> the catalog entry -> its adapter must keep the loop's allocation.
#[tokio::test(flavor = "current_thread")]
async fn loop_request_through_the_catalog_never_copies_the_transcript() {
    let adapter = Arc::new(RecordingAdapter::default());
    let mut registry = ProviderRegistry::default();
    registry.set_provider(catalog_with(Arc::clone(&adapter)));
    let registry = Arc::new(registry);

    let deps = LoopDeps {
        max_turns: 1,
        ..LoopDeps::default()
    }
    .with_provider_unchecked(
        Arc::clone(&registry),
        test_model(),
        StreamOptions::default(),
    );

    let result = run_with_options(deps, user_context("hello"), None, LoopOptions::default()).await;

    let loop_ptr = Arc::as_ptr(&result.context) as usize;
    assert_eq!(
        adapter.shared_ptrs(),
        vec![loop_ptr],
        "the catalog wrapper must delegate the loop's Arc<Context> to the adapter's \
         stream_shared (a deep copy would record a different allocation)"
    );
    assert_eq!(
        adapter.owned_calls(),
        0,
        "the catalog wrapper must not fall back to Provider::stream"
    );
}

/// Minimal no-network OAuth flow: the overlay only needs the trait object.
struct NoopFlow;

impl OAuthFlow for NoopFlow {
    fn name(&self) -> &str {
        "Noop"
    }

    fn login<'a>(
        &'a self,
        _interaction: &'a dyn AuthInteraction,
        _options: &'a LoginOptions,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async { Err(AiError::NonRetryable("unused".into())) })
    }

    fn refresh<'a>(
        &'a self,
        _credential: &'a OAuthCredential,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async { Err(AiError::NonRetryable("unused".into())) })
    }

    fn to_auth(&self, _credential: &OAuthCredential) -> AiResult<ModelAuth> {
        Err(AiError::NonRetryable("unused".into()))
    }
}

#[test]
fn oauth_flow_overlay_forwards_the_shared_context_handle() {
    let adapter = Arc::new(RecordingAdapter::default());
    let flow: OAuthFlowBox = Arc::new(NoopFlow);
    let wrapped = with_oauth_flow(Arc::clone(&adapter) as ProviderBox, flow);
    assert!(wrapped.oauth_flow().is_some(), "overlay exposes the flow");

    let context = Arc::new(user_context("hello"));
    let context_ptr = Arc::as_ptr(&context) as usize;
    // The recording adapter pushes its terminal event before returning, so
    // the returned stream is already complete here.
    let _stream = wrapped.stream_shared(
        &test_model(),
        Arc::clone(&context),
        StreamOptions::default(),
    );

    assert_eq!(
        adapter.shared_ptrs(),
        vec![context_ptr],
        "the OAuth overlay must forward stream_shared, not re-materialize the context"
    );
    assert_eq!(adapter.owned_calls(), 0);
}
