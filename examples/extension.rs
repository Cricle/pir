//! Load a JS extension from disk and let the model call its tool.
//!
//! `extension_paths` points the embedded QuickJS runtime at a `.js` file. The
//! file's default export receives the `pi` API; `pi.registerTool` lands on the
//! same plugin host as Rust tools and builtins. The example writes the
//! extension to a temporary directory first, so the checkout stays clean and
//! the run stays offline.
//!
//! ```text
//! cargo run --example extension --no-default-features \
//!   --features "sdk,extensions"
//! ```
//!
//! The `extensions` cargo feature compiles the QuickJS runtime. Without it,
//! an on-disk extension path fails `build()` with an explicit "requires the JS
//! extension runtime" error.

mod common;

use std::path::PathBuf;

use pir::ai::types::ThinkingLevel;
use pir::sdk::{AgentBuilder, SdkError};

/// A tiny extension: one tool, registered at load time.
const SHOUT_EXTENSION: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "js_shout",
    description: "Upper-case text using the embedded JS runtime",
    parameters: {
      type: "object",
      properties: {
        text: { type: "string", description: "Text to upper-case" }
      },
      required: ["text"]
    },
    async execute(_toolCallId, params) {
      return {
        content: [{ type: "text", text: String(params.text).toUpperCase() }]
      };
    }
  });
}
"#;

#[tokio::main]
async fn main() -> Result<(), SdkError> {
    // Disposable workspace: the extension file and the agent directory live
    // here, never in the checkout or `~/.pi`.
    let dir: PathBuf =
        std::env::temp_dir().join(format!("pir-sdk-extension-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir)?;
    let extension_path = dir.join("shout.js");
    std::fs::write(&extension_path, SHOUT_EXTENSION)?;

    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::tool_calls_message(&[(
            "js_shout",
            serde_json::json!({ "text": "hello from quickjs" }),
        )]),
        common::text_message("The JS tool returned the upper-cased text."),
    ]);

    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        // Explicit paths are loaded strictly: a broken extension fails
        // `build()` instead of leaving a silently incomplete agent.
        .extension_paths(vec![extension_path.clone()])
        .agent_dir(agent_dir)
        .cwd(dir.clone())
        .system_prompt("You are a tiny offline example agent.")
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
    println!("extension: {}", extension_path.display());
    println!("available tools: {}", tools.join(", "));

    let outcome =
        common::prompt_outcome(&agent, "Run js_shout on the text `hello from quickjs`.").await?;
    println!("assistant: {}", outcome.message_text().unwrap_or_default());
    for result in outcome.tool_result_texts() {
        println!("tool result: {result}");
    }

    agent.shutdown().await?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
