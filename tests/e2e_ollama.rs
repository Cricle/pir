//! End-to-end integration test with local ollama.
//! Requires ollama running with qwen3:0.6b model on localhost:11434.
//! Tests the full OpenAI-compatible adapter through the provider registry.

use futures_util::StreamExt;
use pir::ai::api::openai::OpenAiCompatibleProvider;
use pir::ai::provider::Provider;
use pir::ai::types::*;
use std::sync::Arc;
use std::time::Duration;

/// Probe the ollama port before running the suite.
///
/// When ollama is absent, the default transport would burn ~8s on connect
/// timeouts × retries; the probe fails fast instead and the tests skip.
fn ollama_available() -> bool {
    std::net::TcpStream::connect_timeout(
        &"127.0.0.1:11434".parse().expect("static addr"),
        Duration::from_millis(250),
    )
    .is_ok()
}

/// Provider with test-appropriate transport: 2s connect timeout, no retries.
fn test_provider() -> Result<Arc<OpenAiCompatibleProvider>, pir::ai::error::AiError> {
    let client = pir::ai::transport::ProviderRequestClient::new(
        pir::ai::transport::ProviderRequestConfig {
            timeout: Duration::from_secs(2),
            max_retries: 0,
            ..pir::ai::transport::ProviderRequestConfig::default()
        },
    )?;
    Ok(Arc::new(
        OpenAiCompatibleProvider::new("ollama", "http://localhost:11434/v1", vec![sample_model()])?
            .with_client(client),
    ))
}

fn sample_model() -> Model {
    Model {
        id: "qwen3:0.6b".into(),
        name: "Qwen 0.6B".into(),
        api: "openai-chat".into(),
        provider: "ollama".into(),
        base_url: "http://localhost:11434/v1".into(),
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
        max_tokens: 1024,
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

#[tokio::test]
async fn ollama_completion_returns_usage() {
    if !ollama_available() {
        return;
    }
    let Ok(provider) = test_provider() else {
        return;
    };
    let model = sample_model();
    let ctx = Context {
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: "Say hello".into(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 0,
            sections: None,
        })],
        ..Context::default()
    };
    let mut stream = provider.stream(&model, ctx, StreamOptions::default());
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    assert!(!events.is_empty(), "should produce events");
    // The stream must terminate in `Done`; an error/other terminal event is
    // NOT a pass (the old `if let` silently skipped the usage assertion).
    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("expected terminal Done, got {:?}", events.last());
    };
    assert!(
        message.usage.total_tokens > 0,
        "usage.total_tokens should be > 0"
    );
}

#[tokio::test]
async fn ollama_completion_returns_stop_reason() {
    if !ollama_available() {
        return;
    }
    let Ok(provider) = test_provider() else {
        return;
    };
    let model = sample_model();
    let ctx = Context {
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: "Say hello world".into(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 0,
            sections: None,
        })],
        ..Context::default()
    };
    let mut stream = provider.stream(&model, ctx, StreamOptions::default());
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("expected terminal Done, got {:?}", events.last());
    };
    assert!(matches!(
        message.stop_reason,
        StopReason::Stop | StopReason::Length
    ));
}
