//! Manual compaction through the SDK: the boundary, the event pair, the
//! JSONL entry, and the next request's context.
//!
//! ```text
//! cargo run --example session_compaction --no-default-features --features "sdk,session"
//! ```

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pir::ai::types::ThinkingLevel;
use pir::compaction::CompactionConfig;
use pir::sdk::{AgentBuilder, SdkError, SdkEvent, SessionMode};

fn main() -> std::process::ExitCode {
    common::run_big_stack(async {
        if let Err(error) = run().await {
            eprintln!("error: {error:?}");
            return std::process::ExitCode::FAILURE;
        }
        std::process::ExitCode::SUCCESS
    })
}

async fn run() -> Result<(), SdkError> {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("pir-sdk-compaction-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let (registry, model, requests) = common::ScriptedProvider::registry_recording(vec![
        common::text_message("first answer with some words"),
        common::text_message("second answer with more words"),
        common::text_message("SUMMARY: the user asked two short questions."),
        // A cut that lands mid-turn also asks for a turn-prefix summary.
        common::text_message("TURN SUMMARY: earlier turn context."),
        common::text_message("post-compaction answer"),
    ]);
    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .system_prompt("You are a tiny offline example agent.")
        .session(SessionMode::Dir(dir.clone()))
        .compaction(CompactionConfig {
            enabled: true,
            // Force a cut even for this tiny transcript: keep only the last
            // token of tail, reserve nearly the whole window.
            reserve_tokens: 32_700,
            keep_recent_tokens: 1,
        })
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;

    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let events_log = Arc::clone(&events);
    agent.on_session_event(move |event| {
        if let Some(event) = SdkEvent::from_session_event(&event) {
            let name = match event {
                SdkEvent::CompactionStart { .. } => "compaction_start",
                SdkEvent::CompactionEnd { .. } => "compaction_end",
                SdkEvent::EntryAppended { .. } => "entry_appended",
                SdkEvent::AgentSettled { .. } => "agent_settled",
                _ => return,
            };
            events_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(name.to_owned());
        }
    });

    agent.prompt("first question").await?;
    agent.prompt("second question").await?;
    let before = agent.transcript().await;
    println!("transcript before compaction: {} messages", before.len());

    let result = agent.compact().await?;
    println!(
        "compaction: tokens_before={} first_kept={:?} retained_tail={} summary={:?}",
        result.tokens_before,
        result.first_kept_entry_id,
        result.retained_tail.len(),
        result.summary.chars().take(40).collect::<String>()
    );
    let after = agent.transcript().await;
    println!("transcript after compaction: {} messages", after.len());
    println!(
        "compaction events: {:?}",
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    );
    assert!(
        result.tokens_before > 0,
        "compaction reports the pre-compaction size"
    );
    assert!(result.first_kept_entry_id.is_some());

    let outcome = agent.prompt("what did I ask before?").await?;
    println!(
        "post-compaction answer: {}",
        outcome.message_text().unwrap_or_default()
    );

    let recorded = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    println!("requests recorded: {}", recorded.len());
    let last = recorded.last().expect("post-compaction request");
    println!("post-compaction user texts: {:?}", common::user_texts(last));

    // The JSONL gets a compaction entry, and the next request carries the
    // summary rather than the whole pre-compaction transcript.
    let path = agent.session_path().expect("persistent path");
    let content = std::fs::read_to_string(&path)?;
    let kinds: Vec<String> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|row| {
            row.get("type")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        })
        .collect();
    println!("session row types: {kinds:?}");
    assert!(
        kinds.iter().any(|kind| kind == "compaction"),
        "manual compaction appends a compaction entry"
    );
    let event_names = events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(
        event_names.iter().any(|name| name == "compaction_start")
            && event_names.iter().any(|name| name == "compaction_end"),
        "the event pair must be observable: {event_names:?}"
    );

    agent.shutdown().await?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
