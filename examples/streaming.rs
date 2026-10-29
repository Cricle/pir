//! Stream one prompt as typed events, including a tool round-trip.
//!
//! The scripted provider first asks for the `echo` tool, then streams a text
//! answer in chunks. Every `SdkEvent` variant is handled here (not every one
//! occurs in this run; the match has a `_` arm because the enum is
//! `#[non_exhaustive]`).
//!
//! ```text
//! cargo run --example streaming --no-default-features \
//!   --features "sdk"
//! ```

mod common;

use std::sync::Arc;

use futures_util::StreamExt;

use pir::ai::types::ThinkingLevel;
use pir::sdk::{AgentBuilder, SdkError, SdkEvent};

/// Name every `SdkEvent` variant this build exposes. Keeping this match
/// explicit is a compile-time checklist: a new SDK event shows up here (and in
/// the `_` arm is only a forward-compat guard, because the enum is
/// `#[non_exhaustive]`).
fn event_name(event: &SdkEvent) -> &'static str {
    match event {
        SdkEvent::TurnStart { .. } => "turn_start",
        SdkEvent::TurnEnd { .. } => "turn_end",
        SdkEvent::MessageStart => "message_start",
        SdkEvent::MessageUpdate { .. } => "message_update",
        SdkEvent::MessageEnd { .. } => "message_end",
        SdkEvent::TranscriptMessageStart { .. } => "transcript_message_start",
        SdkEvent::TranscriptMessageEnd { .. } => "transcript_message_end",
        SdkEvent::ToolCallStart { .. } => "tool_call_start",
        SdkEvent::ToolCallDelta { .. } => "tool_call_delta",
        SdkEvent::ToolCallEnd { .. } => "tool_call_end",
        SdkEvent::MessageDone { .. } => "message_done",
        SdkEvent::MessageError { .. } => "message_error",
        SdkEvent::ToolExecutionStart { .. } => "tool_execution_start",
        SdkEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
        SdkEvent::ToolExecutionEnd { .. } => "tool_execution_end",
        SdkEvent::Compaction => "compaction",
        SdkEvent::QueueUpdate { .. } => "queue_update",
        SdkEvent::CompactionStart { .. } => "compaction_start",
        SdkEvent::CompactionEnd { .. } => "compaction_end",
        SdkEvent::EntryAppended { .. } => "entry_appended",
        SdkEvent::SessionInfoChanged { .. } => "session_info_changed",
        SdkEvent::ThinkingLevelChanged { .. } => "thinking_level_changed",
        SdkEvent::BashExecutionUpdate { .. } => "bash_execution_update",
        SdkEvent::AutoRetryStart { .. } => "auto_retry_start",
        SdkEvent::AutoRetryEnd { .. } => "auto_retry_end",
        SdkEvent::SummarizationRetryScheduled { .. } => "summarization_retry_scheduled",
        SdkEvent::SummarizationRetryAttemptStart { .. } => "summarization_retry_attempt_start",
        SdkEvent::SummarizationRetryFinished => "summarization_retry_finished",
        SdkEvent::BackgroundToolEnd { .. } => "background_tool_end",
        SdkEvent::PromptRejected { .. } => "prompt_rejected",
        SdkEvent::AgentEnd { .. } => "agent_end",
        SdkEvent::AgentOutcome(_) => "agent_outcome",
        SdkEvent::AgentSettled { .. } => "agent_settled",
        _ => "future_event",
    }
}

#[tokio::main]
async fn main() -> Result<(), SdkError> {
    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::tool_calls_message(&[(
            "echo",
            serde_json::json!({ "text": "hello from the tool" }),
        )]),
        common::text_message(
            "The echo tool ran. This answer is streamed in small chunks, so MessageUpdate \
             events arrive before the final message.",
        ),
    ]);

    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .system_prompt("You are a tiny offline example agent.")
        .with_tool(Arc::new(common::EchoTool))
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;

    // `prompt_stream_with_outcome` is `prompt_stream` plus one final
    // `SdkEvent::AgentOutcome` carrying the complete run result.
    let mut events = agent.prompt_stream_with_outcome("Call echo, then explain what happened.");
    while let Some(event) = events.next().await {
        match event {
            SdkEvent::TurnStart { turn_index } => println!("[{turn_index}] turn start"),
            SdkEvent::MessageStart => println!("    message start"),
            SdkEvent::MessageUpdate { delta, thinking } => {
                if thinking {
                    print!("    [thinking] {delta}");
                } else {
                    print!("    {delta}");
                }
            }
            SdkEvent::MessageEnd { message } => {
                println!("\n    message end (stop_reason={:?})", message.stop_reason);
            }
            SdkEvent::ToolExecutionStart {
                tool_name, args, ..
            } => println!("    tool start: {tool_name} args={args}"),
            SdkEvent::ToolExecutionUpdate {
                tool_name, partial, ..
            } => println!("    tool update: {tool_name:?} partial={partial}"),
            SdkEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                duration_ms,
                ..
            } => println!("    tool end: {tool_name} error={is_error} duration_ms={duration_ms:?}"),
            SdkEvent::TurnEnd {
                turn_index,
                message,
                ..
            } => println!(
                "[{turn_index}] turn end (stop_reason={:?})",
                message.stop_reason
            ),
            SdkEvent::Compaction => println!("    [compaction ran]"),
            SdkEvent::PromptRejected { message } => println!("    [prompt rejected] {message}"),
            SdkEvent::AgentOutcome(outcome) => {
                println!(
                    "[outcome] text={:?} turns={} aborted={}",
                    outcome.message_text().unwrap_or_default(),
                    outcome.turns,
                    outcome.aborted
                );
            }
            SdkEvent::AgentSettled { aborted } => {
                println!("[settled] aborted={aborted}");
                break;
            }
            other => println!("    [{}]", event_name(&other)),
        }
    }

    // `AgentOutcome` (above) carries this run's usage. `usage_totals()` is a
    // session-level counter across every prompt run; it advances with
    // `no_session()` too (here: 1 tool turn + 1 final-answer turn = 2).
    common::print_usage(&agent);
    agent.shutdown().await?;
    Ok(())
}
