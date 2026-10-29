//! Attach a compiled-in builtin and inspect the merged tool registry.
//!
//! `.builtin("subagent")` resolves through the same plugin host as inline
//! plugins and JS extensions. The builtin's native tool is registered during
//! `build()`, so `Agent::tools()` shows exactly what the model will be
//! offered. The example never calls the tool, so it stays offline and
//! deterministic.
//!
//! ```text
//! cargo run --example builtin --no-default-features \
//!   --features "sdk,subagent"
//! ```
//!
//! The `subagent` cargo feature is what compiles the builtin in. Without it,
//! `build()` fails with the "not compiled into this build" diagnostic instead
//! of silently producing an agent without the tool.

mod common;

use std::path::PathBuf;

use pir::ai::types::ThinkingLevel;
use pir::sdk::{AgentBuilder, SdkError};

#[tokio::main]
async fn main() -> Result<(), SdkError> {
    // Keep every agent-dir read inside a disposable directory.
    let dir: PathBuf = std::env::temp_dir().join(format!("pir-sdk-builtin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir)?;

    let (registry, model) = common::ScriptedProvider::registry(vec![common::text_message(
        "The subagent builtin is attached; this answer came from the scripted provider.",
    )]);

    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir.clone())
        .cwd(dir.clone())
        .system_prompt("You are a tiny offline example agent.")
        // Resolve a compiled-in builtin by name. `builtin:subagent` is the
        // equivalent prefixed form.
        .builtin("subagent")
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;

    // The live declaration view: builder tools, plugin tools, and builtin
    // tools after the builder's tool selection has been applied. Sorted by
    // name, so the output is stable. `ToolInfo` also carries the model-facing
    // `description` and the JSON `parameters` schema.
    println!("tools visible to the model:");
    for tool in agent.tools() {
        let schema = if tool.parameters.is_some() {
            "json schema"
        } else {
            "no parameters"
        };
        println!("  {} ({schema})", tool.name);
    }

    let outcome = common::prompt_outcome(&agent, "Which tools do you have?").await?;
    println!("assistant: {}", outcome.message_text().unwrap_or_default());

    agent.shutdown().await?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
