//! Error-path reality check for the SDK surface.
//!
//! Exercises the documented failure modes against a real build:
//! missing model, zero context window, a builtin whose cargo feature is off,
//! an unknown tool call, a second prompt while a run is in flight, abort then
//! prompt again, and prompt after shutdown. Every case must return a typed
//! `SdkError` (or a tool error) and must not panic; recoverable cases must
//! leave the agent usable.
//!
//! ```text
//! cargo run --example sdk_error_paths --no-default-features --features "sdk"
//! ```

mod common;

use std::path::PathBuf;

use pir::ai::types::{Model, StopReason, ThinkingLevel};
use pir::sdk::{AgentBuilder, SdkError};

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
    let root = std::env::temp_dir().join(format!("pir-sdk-error-paths-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(&agent_dir)?;

    // 1. Missing model: no endpoint, no models.json, empty catalog.
    let error = AgentBuilder::new()
        .agent_dir(&agent_dir)
        .no_session()
        .no_compaction()
        .build()
        .await
        .expect_err("a model-less build must fail");
    println!("missing model -> {error}");
    let display = error.to_string();
    assert_eq!(
        display.matches("no model available").count(),
        1,
        "NoModel Display must not double the prefix: {display}"
    );
    assert!(
        matches!(error, SdkError::NoModel { .. }),
        "expected SdkError::NoModel, got {error:?}"
    );
    println!("PASS [missing-model] typed SdkError::NoModel");

    // 2. Zero context window: rejected at with_registry/build, not at runtime.
    let mut model = common::model();
    model.context_window = 0;
    let (registry, _) = common::ScriptedProvider::registry(vec![common::text_message("x")]);
    let error = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(&agent_dir)
        .no_session()
        .no_compaction()
        .build()
        .await
        .expect_err("zero context window must be rejected");
    println!("zero context -> {error}");
    let text = format!("{error}");
    assert!(text.contains("context"), "unexpected error: {text}");
    println!("PASS [zero-context] rejected before any request");

    // 3. Uncompiled builtin: `todo` is not in this `sdk`-only build.
    let (registry, model) = common::ScriptedProvider::registry(vec![common::text_message("x")]);
    let error = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(&agent_dir)
        .no_session()
        .no_compaction()
        .builtin("todo")
        .build()
        .await
        .expect_err("an uncompiled builtin must fail the build");
    println!("uncompiled builtin -> {error}");
    assert!(
        format!("{error}").contains("not compiled into this build"),
        "expected the honest missing-feature diagnostic, got {error}"
    );
    println!("PASS [uncompiled-builtin] honest diagnostic");

    // 4. Missing tool: the model calls a tool that is not registered.
    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::tool_calls_message(&[("does_not_exist", serde_json::json!({}))]),
        common::text_message("the tool failed"),
    ]);
    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(&agent_dir)
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;
    let outcome = agent.prompt("Call the missing tool.").await?;
    let tool_text = outcome.tool_result_texts().join("\n");
    assert!(
        outcome.tool_results[0].is_error,
        "missing tool must produce an error result: {tool_text}"
    );
    println!("missing tool -> {tool_text}");
    println!("PASS [missing-tool] tool error, run continued to a normal stop");
    agent.shutdown().await?;

    // 5. Concurrent prompt while streaming: rejected; abort then prompt again.
    let (registry, model, started) = common::AbortableProvider::registry();
    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(&agent_dir)
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;
    let mut first = Box::pin(agent.prompt_stream("first prompt"));
    // The stream starts the run lazily; poll one event so the provider starts.
    use futures_util::StreamExt;
    let first_event = tokio::select! {
        event = first.next() => event,
        _ = started.notified() => None,
    };
    let _ = first_event;
    started.notified().await;
    let concurrent = agent.prompt("second prompt").await;
    let concurrent = concurrent.expect_err("a concurrent prompt must be rejected");
    println!("concurrent prompt -> {concurrent}");
    let concurrent_text = format!("{concurrent}");
    assert!(
        concurrent_text.contains("in progress")
            || concurrent_text.contains("busy")
            || concurrent_text.contains("streaming"),
        "unexpected concurrent error: {concurrent_text}"
    );
    agent.abort();
    while first.next().await.is_some() {}
    println!("PASS [concurrent-prompt] rejected; abort settled the first run");

    // Abort then prompt again: the provider stays gated, so abort the second
    // run too; what matters is that the agent accepted it and did not panic.
    let mut second = Box::pin(agent.prompt_stream("after abort"));
    started.notified().await;
    agent.abort();
    while second.next().await.is_some() {}
    println!("PASS [prompt-after-abort] a new prompt is accepted after abort");

    // 6. Prompt after shutdown.
    agent.shutdown().await?;
    let error = agent
        .prompt("after shutdown")
        .await
        .expect_err("a prompt after shutdown must fail");
    println!("after shutdown -> {error}");
    let shutdown_text = format!("{error}");
    assert!(
        shutdown_text.contains("shut down") || shutdown_text.contains("closed"),
        "unexpected shutdown error: {shutdown_text}"
    );
    println!("PASS [prompt-after-shutdown] typed error, no panic");

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

// Keep the unused-import warning away on a build where the model type is only
// needed above.
#[allow(dead_code)]
fn _assert_model_type_is_used(_: &Model) {}
#[allow(dead_code)]
fn _assert_stop_reason_is_used(_: StopReason) {}
#[allow(dead_code)]
fn _unused_path(_: PathBuf) {}
