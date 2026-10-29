//! Tools, three ways: built-ins, a typed `AgentTool`, and an async closure.
//!
//! The scripted provider asks for two custom tools in one batch, then answers.
//! The typed tool also emits a progress update before it returns.
//!
//! ```text
//! # Minimal (custom tools only):
//! cargo run --example tools --no-default-features --features "sdk"
//! # Add `tools` to also see the built-in coding tools:
//! cargo run --example tools --no-default-features --features "sdk,tools"
//! ```
//!
//! The `tools` feature is what compiles in the built-in coding tools. The
//! example follows the build (the flag is `cfg!(feature = "tools")`); a
//! tool-less build registers only the two custom tools. Requesting
//! `core_tools(true)` in a build with no compiled core tool fails `build()`
//! with the explicit missing-feature diagnostic instead of silently
//! registering nothing.

mod common;

use std::sync::Arc;

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use pir::ai::types::ThinkingLevel;
use pir::sdk::{AgentBuilder, SdkError};

/// A typed tool: full control over the schema, the async body, and progress
/// updates. This is the shape to use for real I/O.
struct WordCount;

impl AgentTool for WordCount {
    fn name(&self) -> &str {
        "word_count"
    }

    fn description(&self) -> &str {
        "Count the whitespace-separated words in a text"
    }

    fn parameters(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to count" }
            },
            "required": ["text"]
        }))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let text = args["text"].as_str().unwrap_or_default().to_owned();
        // `ToolContext` is how a typed tool sees its execution environment:
        // `cwd` is the session's working directory, and `is_aborted()` reports
        // a host `Agent::abort()` (check it periodically in real I/O loops).
        let cwd = ctx.cwd.clone();
        let aborted = ctx.is_aborted();
        let update = ctx.update_channel();
        Box::pin(async move {
            if aborted {
                return ToolResult::error(
                    "word_count cancelled before it started: the run was aborted",
                );
            }
            if let Some(update) = update {
                update(json!({
                    "content": [{
                        "type": "text",
                        "text": format!("counting words in {}", cwd.display())
                    }],
                    "isError": false
                }));
            }
            let count = text.split_whitespace().count();
            ToolResult::ok(format!("{count} words"))
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), SdkError> {
    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::tool_calls_message(&[
            (
                "word_count",
                json!({ "text": "the quick brown fox jumps over the lazy dog" }),
            ),
            ("shout", json!({ "text": "two tools in one batch" })),
        ]),
        common::text_message(
            "word_count returned 9 and shout returned the upper-cased text. Both calls ran in \
             one tool batch.",
        ),
    ]);

    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .system_prompt("You are a tiny offline example agent.")
        // Built-ins: read/write/edit/bash/ls and the two background controls
        // are default-active; `powershell` is registered but starts inactive,
        // so it is absent from this declaration view. They only exist when the
        // `tools` feature is compiled in, so the flag follows the build instead
        // of claiming tools that are absent. Disabled by default in every
        // build.
        .core_tools(cfg!(feature = "tools"))
        // Typed tool.
        .with_tool(Arc::new(WordCount))
        // Closure tool: the ergonomic one-off path.
        .tool(
            "shout",
            "Upper-case the provided text",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Text to upper-case" }
                },
                "required": ["text"]
            }),
            |args| async move {
                let text = args["text"].as_str().unwrap_or_default();
                ToolResult::ok(text.to_uppercase())
            },
        )
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;

    if cfg!(feature = "tools") {
        println!("core tools: compiled in");
    } else {
        println!("core tools: off (build with the `tools` feature to add them)");
    }

    // Introspection: the merged registry the model sees.
    let tools = agent
        .tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    println!("available tools: {}", tools.join(", "));

    let outcome =
        common::prompt_outcome(&agent, "Count the words and shout the other text.").await?;
    println!("assistant: {}", outcome.message_text().unwrap_or_default());
    for (index, result) in outcome.tool_result_texts().iter().enumerate() {
        println!("tool result {}: {result}", index + 1);
    }
    println!(
        "run: turns={} tool_results={}",
        outcome.turns,
        outcome.tool_results.len()
    );

    agent.shutdown().await?;
    Ok(())
}
