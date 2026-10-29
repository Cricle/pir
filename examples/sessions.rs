//! Persistent sessions: create, name, rotate, and resume.
//!
//! The agent writes the upstream JSONL session format, so the file is also
//! readable by pi. This example runs entirely offline against the scripted
//! provider.
//!
//! ```text
//! cargo run --example sessions --no-default-features \
//!   --features "sdk,session"
//! ```

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pir::ai::types::ThinkingLevel;
use pir::sdk::{AgentBuilder, SdkEvent, SessionMode};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("pir-sdk-sessions-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::text_message("first turn"),
        common::text_message("second turn in a fresh session"),
        common::text_message("resumed turn from the first session"),
    ]);

    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .system_prompt("You are a tiny offline example agent.")
        .session(SessionMode::Dir(dir.clone()))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;

    // --- event subscription -------------------------------------------------
    // `on_session_event` is the facade-level subscription to events that are
    // not tied to one prompt stream (especially useful with `prompt()`, which
    // has no SdkEvent stream). Here we project each SessionEvent onto the typed
    // SdkEvent surface and record the kind; `None` is only the session settle.
    let observed = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_log = Arc::clone(&observed);
    agent.on_session_event(move |event| {
        let kind = match SdkEvent::from_session_event(&event) {
            Some(SdkEvent::EntryAppended { .. }) => "entry_appended",
            Some(SdkEvent::SessionInfoChanged { .. }) => "session_info_changed",
            Some(SdkEvent::QueueUpdate { .. }) => "queue_update",
            Some(SdkEvent::CompactionStart { .. }) => "compaction_start",
            Some(SdkEvent::CompactionEnd { .. }) => "compaction_end",
            Some(SdkEvent::ThinkingLevelChanged { .. }) => "thinking_level_changed",
            Some(SdkEvent::BashExecutionUpdate { .. }) => "bash_execution_update",
            Some(SdkEvent::AutoRetryStart { .. }) => "auto_retry_start",
            Some(SdkEvent::AutoRetryEnd { .. }) => "auto_retry_end",
            Some(SdkEvent::SummarizationRetryScheduled { .. }) => "summarization_retry_scheduled",
            Some(SdkEvent::SummarizationRetryAttemptStart { .. }) => {
                "summarization_retry_attempt_start"
            }
            Some(SdkEvent::SummarizationRetryFinished) => "summarization_retry_finished",
            Some(SdkEvent::BackgroundToolEnd { .. }) => "background_tool_end",
            Some(_) => "sdk_event",
            None => "agent_settled",
        };
        observed_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(kind.to_owned());
    });

    // --- first session -----------------------------------------------------
    let outcome = common::prompt_outcome(&agent, "first prompt").await?;
    println!("first: {:?}", outcome.message_text().unwrap_or_default());
    let first_path = agent
        .session_path()
        .expect("SessionMode::Dir must have a session path");
    println!("first session path: {}", first_path.display());
    println!("first session id: {:?}", agent.session_id());

    let _ = agent.set_session_name(Some("sdk-example")).await?;
    println!("session name: {:?}", agent.session_name().await);
    println!(
        "session events observed: {:?}",
        observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    );

    // --- rotate to a new session file --------------------------------------
    agent.new_session().await?;
    let outcome = common::prompt_outcome(&agent, "second prompt").await?;
    println!("second: {:?}", outcome.message_text().unwrap_or_default());
    println!(
        "second session path: {:?}",
        agent.session_path().map(|path| path.display().to_string())
    );
    println!("second session id: {:?}", agent.session_id());

    // --- resume the first session ------------------------------------------
    // `attach_session` replays the previous transcript; the next prompt
    // continues it (same runtime, tools, and provider).
    agent.attach_session(&first_path).await?;
    let outcome = common::prompt_outcome(&agent, "third prompt").await?;
    println!("resumed: {:?}", outcome.message_text().unwrap_or_default());
    println!(
        "resumed session path: {:?}",
        agent.session_path().map(|path| path.display().to_string())
    );
    println!("resumed session id: {:?}", agent.session_id());
    common::print_usage(&agent);

    agent.shutdown().await?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
