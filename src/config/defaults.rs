//! Runtime defaults shared by the settings accessors and the published JSON
//! Schema (upstream `core/settings-defaults.ts`).

/// Default HTTP idle timeout (upstream `core/http-dispatcher.ts`
/// `DEFAULT_HTTP_IDLE_TIMEOUT_MS = 300_000`): bounds the wait for response
/// headers and the wait between body reads; `0` (and `"disabled"`) disables
/// the timeout at the transport layer.
pub const HTTP_IDLE_TIMEOUT_MS: u64 = 300_000;

/// Default per-prompt agent-loop turn cap of the non-interactive hosts
/// (print, RPC, server, SDK).
///
/// A "turn" is one model request (one assistant message); every tool call in
/// that message executes in ONE batch and consumes no extra turn. The cap is
/// therefore a runaway guard for a model that keeps requesting tools, not a
/// tool-call budget: a prompt whose model emits one call per assistant
/// message spends one turn per call (the measured soak shape — 100 calls per
/// prompt hit the old 100 cap exactly and never produced a final answer).
///
/// 1000 keeps the old guard's intent while giving a 100-call prompt ~10×
/// headroom for retries/follow-up requests. The interactive TUI keeps the
/// smaller [`INTERACTIVE_AGENT_MAX_TURNS`] because a user can steer a stuck
/// run; non-interactive hosts cannot. `PIR_MAX_TURNS` overrides this value
/// for every non-interactive host (see [`crate::config::env::max_turns`]).
pub const DEFAULT_AGENT_MAX_TURNS: usize = 1_000;

/// The interactive TUI's per-prompt turn cap: pir's original 100-turn
/// runaway guard (upstream's loop itself has no cap). Unchanged on purpose —
/// the non-interactive raise must not silently extend an interactive run.
pub const INTERACTIVE_AGENT_MAX_TURNS: usize = 100;

/// Upstream `SETTINGS_DEFAULTS` (settings-defaults.ts:4-66).
pub mod settings_defaults {
    pub const TRANSPORT: &str = "auto";
    /// Destination of `/share` uploads (upstream
    /// `SETTINGS_DEFAULTS.sessionShareDestination`, `cea257439`): Radius when
    /// authenticated, otherwise a secret GitHub gist.
    pub const SESSION_SHARE_DESTINATION: &str = "auto";
    pub const STEERING_MODE: &str = "one-at-a-time";
    pub const FOLLOW_UP_MODE: &str = "one-at-a-time";
    pub const COMPACTION_ENABLED: bool = true;
    pub const COMPACTION_RESERVE_TOKENS: u64 = 16_384;
    pub const COMPACTION_KEEP_RECENT_TOKENS: u64 = 20_000;
    pub const BRANCH_SUMMARY_RESERVE_TOKENS: u64 = 16_384;
    pub const BRANCH_SUMMARY_SKIP_PROMPT: bool = false;
    pub const RETRY_ENABLED: bool = true;
    pub const RETRY_MAX_RETRIES: u32 = 3;
    pub const RETRY_BASE_DELAY_MS: u64 = 2_000;
    pub const RETRY_MAX_AGENT_DELAY_MS: u64 = crate::ai::util::DEFAULT_MAX_AGENT_RETRY_DELAY_MS;
    pub const RETRY_PROVIDER_MAX_RETRY_DELAY_MS: u64 = 60_000;
    pub const HIDE_THINKING_BLOCK: bool = false;
    pub const SHOW_CACHE_MISS_NOTICES: bool = false;
    pub const QUIET_STARTUP: &str = "false";
    pub const DEFAULT_PROJECT_TRUST: &str = "ask";
    pub const COLLAPSE_CHANGELOG: bool = false;
    pub const ENABLE_INSTALL_TELEMETRY: bool = true;
    pub const ENABLE_ANALYTICS: bool = false;
    pub const ENABLE_SKILL_COMMANDS: bool = true;
    pub const TERMINAL_SHOW_IMAGES: bool = true;
    pub const TERMINAL_IMAGE_WIDTH_CELLS: u32 = 60;
    pub const TERMINAL_CLEAR_ON_SHRINK: bool = false;
    pub const TERMINAL_SHOW_TERMINAL_PROGRESS: bool = false;
    pub const IMAGES_AUTO_RESIZE: bool = true;
    pub const IMAGES_BLOCK_IMAGES: bool = false;
    pub const DOUBLE_ESCAPE_ACTION: &str = "tree";
    pub const TREE_FILTER_MODE: &str = "default";
    pub const EDITOR_PADDING_X: u32 = 0;
    pub const OUTPUT_PAD: u8 = 1;
    pub const AUTOCOMPLETE_MAX_VISIBLE: u32 = 5;
    pub const MARKDOWN_CODE_BLOCK_INDENT: &str = "  ";
    pub const MARKDOWN_MERMAID: &str = "streaming";
    pub const WARNINGS_ANTHROPIC_EXTRA_USAGE: bool = true;
    pub const CODEMODE_MODE: &str = "on";
    pub const CODEMODE_INLINE_BUDGET: u32 = 3_000;
    /// pir-only: the embedded-RustPython `python` tool registers by default;
    /// `pythonTool: false` skips it.
    pub const PYTHON_TOOL_ENABLED: bool = true;
    pub const CACHE_WARMING: &str = "streaming";
    pub const TUI_MODE: &str = "fullscreen";
    pub const FULLSCREEN_EXIT_OUTPUT: &str = "transcript";
    pub const FULLSCREEN_SCROLLBAR: &str = "auto";
    pub const FULLSCREEN_COPY_ON_SELECT: bool = true;
    pub const FULLSCREEN_WHEEL_SCROLL_LINES: &str = "auto";
}
