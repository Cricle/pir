//! Route model requests through an application proxy server.
//!
//! `ProxyProvider` posts to `{proxy_url}/api/stream` with a bearer token; the
//! proxy owns the provider credentials. This is the Rust equivalent of
//! upstream's `streamProxy`.
//!
//! ```text
//! set PIR_PROXY_URL=https://genai.example.com
//! set PIR_PROXY_TOKEN=...
//! set PIR_PROXY_MODEL=my-model
//! cargo run --example proxy --no-default-features \
//!   --features "sdk"
//! ```

mod common;

use pir::sdk::{AgentBuilder, ProxyEndpoint, ProxyProvider};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proxy_url =
        std::env::var("PIR_PROXY_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let auth_token = std::env::var("PIR_PROXY_TOKEN").unwrap_or_default();
    let model_id = std::env::var("PIR_PROXY_MODEL").unwrap_or_else(|_| "proxy-model".into());

    let mut model = common::model();
    model.id = model_id.clone();
    model.name = model_id.clone();
    model.provider = "proxy".into();
    model.api = "proxy".into();

    let provider = ProxyProvider::new(
        ProxyEndpoint::new(proxy_url, auth_token),
        vec![model.clone()],
    )?;

    let agent = AgentBuilder::new()
        .proxy(provider, model)
        .system_prompt("You are a concise assistant.")
        .no_session()
        .build()
        .await?;

    let outcome = common::prompt_outcome(
        &agent,
        "Explain in one sentence why a proxy can keep provider keys off the client.",
    )
    .await?;
    match outcome.message_text() {
        Some(text) => println!("assistant: {text}"),
        None => println!(
            "no assistant text: stop_reason={:?} error={:?}",
            outcome.message.stop_reason, outcome.message.error_message
        ),
    }
    agent.shutdown().await?;
    Ok(())
}
