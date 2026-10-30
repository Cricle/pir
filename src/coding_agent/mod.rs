//! Coding agent: built-in tools, CLI modes, and RPC handlers.
//!
//! Rust equivalent of the upstream `packages/coding-agent/src`.

// Startup bootstrap of the pi-compatible agent directory (`auth.json`,
// `models-store.json`); see the module docs for the upstream evidence.
pub mod agent_bootstrap;
// The `/bug` bundle + Radius upload is driven entirely by the TUI
// (`src/tui/terminal.rs`); no non-TUI production path calls it.
#[cfg(feature = "tui")]
pub mod bug_report;
pub mod cli;
pub mod context;
// The crash journal is installed by the CLI host (`main_cli/run_mode.rs`) and
// surfaced by the TUI; SDK/headless embeds never install or read it.
#[cfg(any(feature = "cli", feature = "tui"))]
pub mod crash_log;
// `--export` and the RPC `export_html` command. The bin and the TUI always
// carry the feature (`cli`/`tui` imply it); an `rpc`-only library build keeps
// the command on the wire and answers the documented off-error.
#[cfg(feature = "export-html")]
pub mod export_html;
pub mod json;
pub mod overrides;
pub mod package_manager;
#[cfg(feature = "print")]
pub mod print;
pub mod prompt_templates;
pub mod resources;
#[cfg(feature = "rpc")]
pub mod rpc;
#[cfg(feature = "tui")]
pub mod session_pick;
pub mod session_resolve;
pub mod tool_selection;
pub mod tools;
pub mod truncate;

// The dependency-free argv helpers stay exported from every build (the print
// host and the extension loader use them through this module); the clap-based
// surface is behind the `cli` feature.
pub use cli::{preprocess_args, Mode};
#[cfg(feature = "cli")]
pub use cli::{Cli, Command, ModelsCommand};
#[cfg(feature = "print")]
pub use print::{
    run_print, truncation_notice, OutputMode, PrintOptions, PrintOutcome, TRUNCATION_WARNING,
};
pub use session_resolve::{resolve_session_plan, SessionFlags, SessionPlan};
pub use tool_selection::{resolve_tool_selection, ToolSelection, ToolSelectionRequest};
pub use tools::register_core_tools;
