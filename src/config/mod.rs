//! Configuration management for the pir agent.

pub mod defaults;
pub mod env;
pub mod first_run;
pub mod keybindings;
pub mod mcp;
pub mod migrations;
pub mod schema;
pub mod settings;
pub mod timings;
pub mod trust;

use serde::{Deserialize, Serialize};

/// Tool execution mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolExecutionMode {
    #[serde(rename = "parallel")]
    #[default]
    Parallel,
    #[serde(rename = "sequential")]
    Sequential,
}

/// Top-level agent configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub tool_execution: ToolExecutionMode,
    #[serde(default = "default_max_parallel_tools")]
    pub max_parallel_tools: usize,
    /// Per-prompt agent-loop turn cap for the non-interactive hosts
    /// (print/RPC/server/SDK): one turn is one model request, and every tool
    /// call in that message executes in one batch. Defaults to
    /// [`defaults::DEFAULT_AGENT_MAX_TURNS`]; `PIR_MAX_TURNS` overrides it
    /// (positive integers only). The interactive TUI keeps its own 100-turn
    /// guard and does not read this field.
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    #[serde(default)]
    pub fail_fast: bool,
    #[serde(default)]
    pub allow_parallel_side_effects: bool,
    /// Default server address.
    #[serde(default = "default_server_address")]
    pub server_address: String,
    /// Session directory path.
    #[serde(default = "default_session_dir")]
    pub session_dir: String,
    /// Optional token required before the remote server handshake.
    #[serde(default)]
    pub server_auth_token: Option<String>,
    /// Maximum number of simultaneous remote server connections.
    #[serde(default = "default_server_max_connections")]
    pub server_max_connections: usize,
    /// Maximum size of one remote request frame in bytes.
    #[serde(default = "default_server_max_request_body_bytes")]
    pub server_max_request_body_bytes: usize,
    /// Maximum bytes accepted from one remote connection.
    #[serde(default = "default_server_max_connection_body_bytes")]
    pub server_max_connection_body_bytes: usize,
    /// Permit remote bash tool execution (reserved for remote tool integration).
    #[serde(default)]
    pub allow_remote_bash: bool,
    /// Permit remote agent/prompt execution.
    #[serde(default = "default_allow_remote_agent")]
    pub allow_remote_agent: bool,
}

fn default_max_parallel_tools() -> usize {
    8
}

fn default_max_turns() -> usize {
    defaults::DEFAULT_AGENT_MAX_TURNS
}

fn default_server_address() -> String {
    "127.0.0.1:4321".to_string()
}

fn default_session_dir() -> String {
    // Upstream layout: `<agent-dir>/sessions/<encoded-cwd>/` (see
    // `session_manager::default_session_dir_path`); `PIR_SESSION_DIR` (read
    // in `from_env`) overrides it.
    crate::session_manager::default_session_dir_path(
        &std::env::current_dir()
            .map(|cwd| cwd.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_owned()),
    )
    .to_string_lossy()
    .into_owned()
}

fn default_server_max_connections() -> usize {
    128
}

fn default_server_max_request_body_bytes() -> usize {
    1024 * 1024
}

fn default_server_max_connection_body_bytes() -> usize {
    64 * 1024 * 1024
}

fn default_allow_remote_agent() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tool_execution: ToolExecutionMode::default(),
            max_parallel_tools: default_max_parallel_tools(),
            max_turns: default_max_turns(),
            fail_fast: false,
            allow_parallel_side_effects: false,
            server_address: default_server_address(),
            session_dir: default_session_dir(),
            server_auth_token: None,
            server_max_connections: default_server_max_connections(),
            server_max_request_body_bytes: default_server_max_request_body_bytes(),
            server_max_connection_body_bytes: default_server_max_connection_body_bytes(),
            allow_remote_bash: false,
            allow_remote_agent: default_allow_remote_agent(),
        }
    }
}

impl Config {
    /// Load configuration from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        let mut config = Config::default();

        if let Ok(val) = std::env::var("PIR_TOOL_EXECUTION") {
            config.tool_execution = match val.as_str() {
                "sequential" => ToolExecutionMode::Sequential,
                _ => ToolExecutionMode::Parallel,
            };
        }
        if let Ok(val) = std::env::var("PIR_MAX_PARALLEL_TOOLS") {
            if let Ok(n) = val.parse::<usize>() {
                if n > 0 {
                    config.max_parallel_tools = n;
                }
            }
        }
        config.max_turns = env::max_turns_from_lookup(|name| std::env::var(name).ok());
        if let Ok(val) = std::env::var("PIR_FAIL_FAST") {
            config.fail_fast = val == "true" || val == "1";
        }
        if let Ok(val) = std::env::var("PIR_ALLOW_PARALLEL_SIDE_EFFECTS") {
            config.allow_parallel_side_effects = val == "true" || val == "1";
        }
        if let Ok(val) = std::env::var("PIR_SERVER_ADDRESS") {
            config.server_address = val;
        }
        if let Ok(val) = std::env::var("PIR_SESSION_DIR") {
            if !val.is_empty() {
                config.session_dir = val;
            }
        }
        if let Ok(val) = std::env::var("PIR_SERVER_AUTH_TOKEN") {
            if !val.is_empty() {
                config.server_auth_token = Some(val);
            }
        }
        if let Ok(val) = std::env::var("PIR_SERVER_MAX_CONNECTIONS") {
            if let Ok(n) = val.parse::<usize>() {
                if n > 0 {
                    config.server_max_connections = n;
                }
            }
        }
        if let Ok(val) = std::env::var("PIR_SERVER_MAX_REQUEST_BODY_BYTES") {
            if let Ok(n) = val.parse::<usize>() {
                if n > 0 {
                    config.server_max_request_body_bytes = n;
                }
            }
        }
        if let Ok(val) = std::env::var("PIR_SERVER_MAX_CONNECTION_BODY_BYTES") {
            if let Ok(n) = val.parse::<usize>() {
                if n > 0 {
                    config.server_max_connection_body_bytes = n;
                }
            }
        }
        if let Ok(val) = std::env::var("PIR_ALLOW_REMOTE_BASH") {
            config.allow_remote_bash = val == "true" || val == "1";
        }
        if let Ok(val) = std::env::var("PIR_ALLOW_REMOTE_AGENT") {
            config.allow_remote_agent = val == "true" || val == "1";
        }

        config
    }

    /// Convert to agent scheduler config.
    pub fn to_scheduler_config(&self) -> crate::agent::ToolSchedulerConfig {
        crate::agent::ToolSchedulerConfig {
            mode: match self.tool_execution {
                ToolExecutionMode::Parallel => {
                    crate::agent::tool_batch::ToolExecutionMode::Parallel
                }
                ToolExecutionMode::Sequential => {
                    crate::agent::tool_batch::ToolExecutionMode::Sequential
                }
            },
            max_parallel: self.max_parallel_tools,
            fail_fast: self.fail_fast,
            allow_parallel_side_effects: self.allow_parallel_side_effects,
            // Upstream has no scheduler-level call timeout (its `toolExecution`
            // option only selects the mode), and no built-in pir host sets one:
            // tools keep their own timers (the shell command timeout, transport
            // deadlines). The field remains a programmatic embedding knob on
            // `ToolSchedulerConfig`; wiring it to a settings key would invent a
            // setting upstream does not have.
            call_timeout: None,
        }
    }
}
