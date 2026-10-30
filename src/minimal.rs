//! The zero-optional-feature minimal agent profile.
//!
//! `cargo build --no-default-features` keeps this module compiled: it is the
//! smallest *runnable* agent the crate offers, built only from core modules
//! (the agent loop/session, one OpenAI-compatible provider, one read tool and
//! an in-memory transcript). The full `pir` binary and every existing public
//! API are untouched.
//!
//! What the profile deliberately supports:
//!
//! - **one provider**: the plain OpenAI-compatible `/chat/completions` wire
//!   adapter (`ai::api::openai::OpenAiCompatibleProvider`). It works against
//!   OpenAI, OpenRouter, Ollama/LM Studio, vLLM and any other server speaking
//!   that protocol; point [`MinimalConfig::base_url`] at it.
//! - **one tool**: `read_file`, confined to the session working directory.
//! - **in-memory session**: no JSONL persistence, no compaction, no skills,
//!   no MCP, no extensions, no TUI. `cargo tree --no-default-features` still
//!   shows the core graph (tokio/reqwest/serde...), but no `cli`, `tui`,
//!   `extensions`, `mcp` or `session` surface.
//!
//! The executable front end is `src/bin/pir-minimal.rs` (`pir-minimal`): it
//! parses the handful of flags/environment variables in [`MinimalConfig`] and
//! runs a one-shot prompt or a stdin REPL.
//!
//! ```text
//! cargo run --no-default-features --bin pir-minimal -- --prompt "hello"
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use crate::agent_session::{AgentSession, AgentSessionConfig};
use crate::ai::api::openai::OpenAiCompatibleProvider;
use crate::ai::auth::{EnvAuthContext, InMemoryCredentialStore};
use crate::ai::provider::ProviderRegistry;
use crate::ai::types::{AssistantMessage, ContentBlock, Model, ThinkingLevel};
use crate::config::defaults::DEFAULT_AGENT_MAX_TURNS;

/// Default endpoint: OpenAI's public API. Any OpenAI-compatible server works.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// Default model id. Override with `--model` / `PIR_MIN_MODEL`.
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";
/// Default system prompt for the minimal profile.
pub const DEFAULT_SYSTEM_PROMPT: &str =
    "You are pir's minimal agent. Be concise. Use the read_file tool when you need file contents.";

/// Configuration for [`MinimalAgent`].
#[derive(Debug, Clone)]
pub struct MinimalConfig {
    /// OpenAI-compatible base URL, including the version segment (e.g.
    /// `https://api.openai.com/v1`).
    pub base_url: String,
    /// Explicit API key. When `None`, the provider resolves `OPENAI_API_KEY`
    /// (and the shell env) the same way every other pir provider does.
    pub api_key: Option<String>,
    /// Model id sent on the wire.
    pub model: String,
    /// System prompt prepended to every request.
    pub system_prompt: String,
    /// Maximum model turns per prompt.
    pub max_turns: usize,
    /// Working directory for the session and the `read_file` tool.
    pub cwd: PathBuf,
    /// Requested reasoning level. Non-reasoning models report `Off`.
    pub thinking: ThinkingLevel,
}

impl Default for MinimalConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_key: None,
            model: DEFAULT_MODEL.to_owned(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_owned(),
            max_turns: DEFAULT_AGENT_MAX_TURNS,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            thinking: ThinkingLevel::Off,
        }
    }
}

/// A ready-to-prompt minimal agent: core `AgentSession` + one provider + one
/// tool + an in-memory transcript.
pub struct MinimalAgent {
    session: Arc<AgentSession>,
    model: Model,
}

impl MinimalAgent {
    /// Build the agent. The returned handle owns the session; call
    /// [`Self::shutdown`] for an explicit plugin teardown (dropping the handle
    /// performs no async work).
    pub async fn build(config: MinimalConfig) -> crate::Result<Self> {
        let model = crate::ai::providers::construction::model(
            "minimal",
            &config.model,
            &config.model,
            "openai-completions",
            &config.base_url,
            128_000,
            8_192,
            vec!["text"],
        );
        let provider =
            OpenAiCompatibleProvider::new("minimal", config.base_url.clone(), vec![model.clone()])?
                .with_display_name("minimal OpenAI-compatible");

        let mut registry = ProviderRegistry::new(
            Arc::new(InMemoryCredentialStore::new()),
            Arc::new(EnvAuthContext::new()),
        );
        registry.set_provider(Arc::new(provider));

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(ReadFileTool));

        let session = AgentSession::new_arc(AgentSessionConfig {
            provider_registry: Arc::new(registry),
            model: model.clone(),
            api_key: config.api_key,
            thinking: config.thinking,
            tools,
            session: None,
            compaction: None,
            max_turns: config.max_turns,
            system_prompt: Some(config.system_prompt),
            cwd: Some(config.cwd),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        });

        Ok(Self { session, model })
    }

    /// The model this agent drives.
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Run one prompt and return the terminal assistant message.
    pub async fn prompt(&self, text: &str) -> crate::Result<AssistantMessage> {
        self.session.prompt(text, Arc::new(|_event| {})).await
    }

    /// Run one prompt, forwarding every [`AgentEvent`](crate::agent::agent_loop::AgentEvent)
    /// to `on_event` (the bin uses this to stream text deltas).
    pub async fn prompt_with_events(
        &self,
        text: &str,
        on_event: crate::agent::agent_loop::EventHandler,
    ) -> crate::Result<AssistantMessage> {
        self.session.prompt(text, on_event).await
    }

    /// Tear the session's plugin surface down (idempotent).
    pub async fn shutdown(&self) {
        self.session.shutdown().await;
    }
}

/// Concatenated text blocks of an assistant message.
pub fn message_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// The single built-in tool of the minimal profile: read a UTF-8 text file
/// under the session working directory.
struct ReadFileTool;

const MAX_READ_BYTES: usize = 256 * 1024;

impl AgentTool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file. Relative paths are resolved against the session working \
         directory; absolute paths must stay inside it. Reads at most 256 KiB."
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path, relative to the session working directory"
                }
            },
            "required": ["path"]
        }))
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let requested = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let cwd = ctx.cwd.clone();
        Box::pin(async move {
            if requested.trim().is_empty() {
                return ToolResult::error("read_file: missing `path`");
            }
            let candidate = {
                let path = Path::new(&requested);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    cwd.join(path)
                }
            };
            let root = tokio::fs::canonicalize(&cwd)
                .await
                .unwrap_or_else(|_| cwd.clone());
            let canonical = match tokio::fs::canonicalize(&candidate).await {
                Ok(path) => path,
                Err(error) => {
                    return ToolResult::error(format!(
                        "read_file: cannot open {}: {error}",
                        candidate.display()
                    ))
                }
            };
            if !canonical.starts_with(&root) {
                return ToolResult::error(format!(
                    "read_file: {} is outside the session working directory",
                    candidate.display()
                ));
            }
            let bytes = match tokio::fs::read(&canonical).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return ToolResult::error(format!(
                        "read_file: cannot read {}: {error}",
                        canonical.display()
                    ))
                }
            };
            let truncated = bytes.len() > MAX_READ_BYTES;
            let slice = &bytes[..bytes.len().min(MAX_READ_BYTES)];
            match std::str::from_utf8(slice) {
                Ok(text) => {
                    if truncated {
                        ToolResult::ok(format!(
                            "{text}\n\n[truncated at {MAX_READ_BYTES} bytes of {}]",
                            bytes.len()
                        ))
                    } else {
                        ToolResult::ok(text)
                    }
                }
                // A truncation cut through a multi-byte character: keep the
                // valid prefix instead of rejecting an otherwise fine text
                // file.
                Err(error) if truncated && error.valid_up_to() > 0 => {
                    // `valid_up_to` is always a UTF-8 boundary; on the
                    // impossible error keep the empty prefix instead of
                    // panicking.
                    let text =
                        std::str::from_utf8(&slice[..error.valid_up_to()]).unwrap_or_default();
                    ToolResult::ok(format!(
                        "{text}\n\n[truncated at {} bytes of {}]",
                        error.valid_up_to(),
                        bytes.len()
                    ))
                }
                Err(_) => ToolResult::error(format!(
                    "read_file: {} is not valid UTF-8 text",
                    canonical.display()
                )),
            }
        })
    }
}

/// Convenience constructor matching the bin's flag surface.
///
/// `api_key` follows the bin precedence: an explicit flag wins, then
/// `PIR_MIN_API_KEY`, then `OPENAI_API_KEY`.
pub fn config_from_env(
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    system_prompt: Option<String>,
    cwd: Option<PathBuf>,
    max_turns: Option<usize>,
) -> MinimalConfig {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    MinimalConfig {
        base_url: base_url
            .or_else(|| env("PIR_MIN_BASE_URL"))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned()),
        api_key: api_key
            .or_else(|| env("PIR_MIN_API_KEY"))
            .or_else(|| env("OPENAI_API_KEY")),
        model: model
            .or_else(|| env("PIR_MIN_MODEL"))
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
        system_prompt: system_prompt
            .or_else(|| env("PIR_MIN_SYSTEM_PROMPT"))
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned()),
        max_turns: max_turns.unwrap_or(DEFAULT_AGENT_MAX_TURNS),
        cwd: cwd
            .or_else(|| env("PIR_MIN_CWD").map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        thinking: ThinkingLevel::Off,
    }
}
