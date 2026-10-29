//! Steering, follow-ups, pending-message counts, and abort.
//!
//! All four controls need no network and no timing guesses:
//!
//! - a steering message queued while the agent is idle is delivered at the
//!   start of the next prompt run (the same poll that runs between tool
//!   turns);
//! - a follow-up queued while idle is delivered after the run's turns finish,
//!   so the same prompt run continues with one more turn;
//! - `pending_message_count()` reports what is still queued;
//! - `abort()` stops an in-flight run. The gated provider in
//!   `examples/common/mod.rs` signals `started` and then waits on the run's
//!   abort signal, so the example aborts at a known point instead of sleeping.
//!
//! ```text
//! cargo run --example control --no-default-features \
//!   --features "sdk"
//! ```

mod common;

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;

use pir::ai::types::{Context, ThinkingLevel};
use pir::sdk::{AgentBuilder, SdkError, SdkEvent};

/// Print the user-message text of every request recorded from `from` onward.
fn print_requests(label: &str, log: &Arc<Mutex<Vec<Context>>>, from: usize) {
    let log = log
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (offset, context) in log.iter().skip(from).enumerate() {
        println!(
            "{label} request log #{}: user messages: {:?}",
            from + offset + 1,
            common::user_texts(context)
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), SdkError> {
    let (registry, model, requests) = common::ScriptedProvider::registry_recording(vec![
        common::text_message("Steering received: I will answer in one sentence."),
        common::text_message("First turn answer."),
        common::text_message("Follow-up turn answer."),
    ]);

    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .system_prompt("You are a tiny offline example agent.")
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;

    // --- steering ---------------------------------------------------------
    println!("== steering ==");
    // Queued while idle: the next prompt run polls the steering inbox before
    // its first model request.
    agent.steer("Answer in one short sentence.");
    println!(
        "pending after steer (idle): {}",
        agent.pending_message_count()
    );

    let outcome = common::prompt_outcome(&agent, "Explain ownership.").await?;
    println!("assistant: {}", outcome.message_text().unwrap_or_default());
    println!("pending after run: {}", agent.pending_message_count());
    print_requests("steer", &requests, 0);

    // --- follow-up --------------------------------------------------------
    println!("== follow-up ==");
    let before_follow_up = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
    // Queued while idle: the next run delivers it after its first finished
    // turn, so the run continues with a second turn.
    agent.follow_up("Now summarize that in three words.");
    println!(
        "pending after follow_up (idle): {}",
        agent.pending_message_count()
    );

    let outcome = common::prompt_outcome(&agent, "Give a long answer.").await?;
    println!(
        "follow-up run: turns={} final_text={:?}",
        outcome.turns,
        outcome.message_text().unwrap_or_default()
    );
    println!("pending after run: {}", agent.pending_message_count());
    print_requests("follow-up", &requests, before_follow_up);

    // Session-level counters still advance with `no_session()`: 1 steering
    // turn plus 2 follow-up turns.
    let usage = agent.usage_totals();
    println!(
        "usage totals: turns={} total_tokens={}",
        usage.turns, usage.total_tokens
    );

    agent.shutdown().await?;

    // --- abort ------------------------------------------------------------
    println!("== abort ==");
    let (registry, model, started) = common::AbortableProvider::registry();
    let abort_agent = AgentBuilder::new()
        .with_registry(registry, model)
        .system_prompt("You are a tiny offline example agent.")
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;

    // Starting the stream starts the run. The provider notifies `started`
    // when it receives the request, so the abort below lands mid-run.
    let mut events = abort_agent.prompt_stream_with_outcome("Wait for an answer.");
    started.notified().await;
    println!("in flight before abort: {}", abort_agent.is_streaming());
    abort_agent.abort();

    let mut aborted = None;
    let mut outcome = None;
    while let Some(event) = events.next().await {
        match event {
            SdkEvent::AgentOutcome(captured) => outcome = Some(*captured),
            SdkEvent::AgentSettled { aborted: settled } => aborted = Some(settled),
            _ => {}
        }
    }
    println!("settled: aborted={aborted:?}");
    if let Some(outcome) = outcome {
        println!(
            "outcome: aborted={} stop_reason={:?}",
            outcome.aborted, outcome.message.stop_reason
        );
    }
    println!(
        "after settle: idle={} pending={}",
        abort_agent.is_idle(),
        abort_agent.pending_message_count()
    );

    abort_agent.shutdown().await?;
    Ok(())
}
