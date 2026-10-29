//! The smallest useful pir SDK program: one prompt, one text answer.
//!
//! It needs no network, no API key, and no `provider-*` cargo feature. The
//! scripted [`Provider`](pir::ai::provider::Provider) lives in
//! `examples/common/mod.rs` — copy that file as the starting point for your
//! own provider.
//!
//! ```text
//! cargo run --example quickstart --no-default-features \
//!   --features "sdk"
//! ```

mod common;

use pir::ai::types::ThinkingLevel;
use pir::sdk::{AgentBuilder, SdkError};

#[tokio::main]
async fn main() -> Result<(), SdkError> {
    // 1. Build a provider + model. No environment resolution is involved:
    //    `with_registry` dispatches straight to this provider.
    let (registry, model) = common::ScriptedProvider::registry(vec![common::text_message(
        "Hello from the pir SDK. This answer came from a scripted Provider.",
    )]);

    // 2. Assemble the agent. Tools are opt-in and `no_session()` keeps the
    //    example hermetic; `no_compaction()` removes the threshold pass and
    //    `thinking_level(Off)` requests no reasoning, so the run is one
    //    request and one answer.
    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .system_prompt("You are a tiny offline example agent.")
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;

    // 3. Run one prompt and inspect the typed outcome. `Agent::prompt` boxes
    //    its future internally, so the direct call is safe on a small start
    //    stack even in unoptimized builds.
    let outcome = agent.prompt("Say hello.").await?;
    println!("assistant: {}", outcome.message_text().unwrap_or_default());
    println!(
        "run: turns={} aborted={} tokens={}",
        outcome.turns, outcome.aborted, outcome.usage.total_tokens
    );

    // 4. Explicit teardown. Dropping the handle performs no async work.
    agent.shutdown().await?;
    Ok(())
}
