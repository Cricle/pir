//! Inline Rust plugin + event hook.
//!
//! `GreetingPlugin` registers a tool on the same plugin host used by JS
//! extensions and compiled-in builtins. `on_event("input", ...)` shows the
//! single-event shortcut: observe (and rewrite) a prompt before it reaches the
//! model.
//!
//! ```text
//! cargo run --example plugin --no-default-features \
//!   --features "sdk"
//! ```

mod common;

use std::sync::Arc;

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use pir::ai::types::ThinkingLevel;
use pir::plugins::{AgentPlugin, InputAction, PluginEventOutcome, PluginInfo, RegistrationContext};
use pir::sdk::{AgentBuilder, SdkError};

/// The tool the plugin registers.
struct GreetTool;

impl AgentTool for GreetTool {
    fn name(&self) -> &str {
        "greet"
    }

    fn description(&self) -> &str {
        "Greet a person by name"
    }

    fn parameters(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Who to greet" }
            },
            "required": ["name"]
        }))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let name = args["name"].as_str().unwrap_or("stranger").to_owned();
        Box::pin(async move { ToolResult::ok(format!("Hello, {name}!")) })
    }
}

struct GreetingPlugin {
    info: PluginInfo,
}

impl GreetingPlugin {
    fn new() -> Self {
        Self {
            info: PluginInfo::inline("greeting-plugin"),
        }
    }
}

impl AgentPlugin for GreetingPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// Called once when the plugin joins the registry. Register tools,
    /// commands, flags, and shortcuts here.
    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(GreetTool));
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), SdkError> {
    let (registry, model, requests) = common::ScriptedProvider::registry_recording(vec![
        common::tool_calls_message(&[("greet", json!({ "name": "Ada" }))]),
        common::text_message("The greet tool said: Hello, Ada!"),
    ]);

    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .system_prompt("You are a tiny offline example agent.")
        .with_plugin(Arc::new(GreetingPlugin::new()))
        // The single-event shortcut. Handlers run in registration order and
        // may transform the prompt in place.
        .on_event("input", |payload| {
            let text = payload["text"].as_str().unwrap_or_default();
            PluginEventOutcome::Input {
                action: InputAction::Transform,
                text: Some(format!("{text} [rewritten by on_event]")),
                images: None,
            }
        })
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;

    let tools = agent
        .tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    println!("available tools: {}", tools.join(", "));

    let outcome = common::prompt_outcome(&agent, "Please greet Ada with the greet tool.").await?;
    // The recorded first request proves the `input` handler rewrote the
    // prompt before it reached the provider.
    if let Some(first) = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
    {
        println!(
            "prompt seen by the provider: {:?}",
            common::user_texts(first)
        );
    }
    println!("assistant: {}", outcome.message_text().unwrap_or_default());
    for result in outcome.tool_result_texts() {
        println!("tool result: {result}");
    }

    agent.shutdown().await?;
    Ok(())
}
