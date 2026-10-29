//! Talk to a real OpenAI-compatible (or Anthropic/Google) endpoint.
//!
//! This is the only example that needs both a network and a matching
//! `provider-*` wire adapter at RUNTIME (`proxy` also needs a network but
//! ships its own provider). The target's `required-features` are the common
//! `sdk,session,skills` set, so it always compiles; add the adapter for the
//! protocol you target when running it:
//!
//! ```text
//! # OpenAI-compatible local server (Ollama, vLLM, LM Studio, ...)
//! cargo run --example real_endpoint --no-default-features \
//!   --features "sdk,session,skills,provider-openai"
//!
//! # a hosted endpoint
//! set PIR_EXAMPLE_BASE_URL=https://api.openai.com/v1
//! set PIR_EXAMPLE_MODEL=gpt-4o-mini
//! set PIR_EXAMPLE_API_KEY=sk-...
//! cargo run --example real_endpoint --no-default-features \
//!   --features "sdk,session,skills,provider-openai"
//! ```
//!
//! Set `PIR_EXAMPLE_API=anthropic` (or `google`) and build with the matching
//! `provider-anthropic` / `provider-google` feature to switch protocols. A
//! run without any adapter still builds but its first request fails with the
//! existing missing-adapter error naming the feature.

mod common;

use pir::sdk::{AgentBuilder, ApiKind, Endpoint, SdkError};

#[tokio::main]
async fn main() -> Result<(), SdkError> {
    let base_url = std::env::var("PIR_EXAMPLE_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:11434/v1".into());
    let model_id = std::env::var("PIR_EXAMPLE_MODEL").unwrap_or_else(|_| "llama3".into());
    let api = match std::env::var("PIR_EXAMPLE_API").as_deref() {
        Ok("anthropic") => ApiKind::AnthropicMessages,
        Ok("google") => ApiKind::GoogleGenerativeAi,
        _ => ApiKind::OpenAiCompletions,
    };

    let mut endpoint = Endpoint::new(base_url, api, model_id);
    endpoint.api_key = std::env::var("PIR_EXAMPLE_API_KEY").ok();
    if let Ok(window) = std::env::var("PIR_EXAMPLE_CONTEXT_WINDOW") {
        if let Ok(window) = window.parse() {
            endpoint.context_window = window;
        }
    }
    if let Ok(max_tokens) = std::env::var("PIR_EXAMPLE_MAX_TOKENS") {
        if let Ok(max_tokens) = max_tokens.parse() {
            endpoint.max_tokens = max_tokens;
        }
    }

    let agent = AgentBuilder::new()
        .endpoint(endpoint)
        .system_prompt("You are a concise assistant. Answer in one sentence.")
        .no_session()
        .build()
        .await?;

    let outcome =
        common::prompt_outcome(&agent, "In one sentence, what is the purpose of an SDK?").await?;
    match outcome.message_text() {
        Some(text) => println!("assistant: {text}"),
        None => println!(
            "no assistant text: stop_reason={:?} error={:?}",
            outcome.message.stop_reason, outcome.message.error_message
        ),
    }
    println!(
        "run: turns={} tokens={}",
        outcome.turns, outcome.usage.total_tokens
    );
    agent.shutdown().await?;
    Ok(())
}
