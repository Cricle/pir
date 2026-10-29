use clap::Parser;

use std::io::IsTerminal;

use pir::coding_agent::cli::{
    lossy_arg, preprocess_args, AppMode, AuthCommand, Cli, Command, McpCommand, Mode, ModelsCommand,
};
use pir::coding_agent::session_resolve::{resolve_session_plan, SessionPlan};
use pir::coding_agent::{OutputMode, PrintOptions};
use pir::config::settings::SettingsManager;
use pir::config::trust::{resolve_run_trust, ProjectTrustStore};
use pir::Result;

#[path = "main_cli/cli_commands.rs"]
mod cli_commands;
#[path = "main_cli/cli_preprocess.rs"]
mod cli_preprocess;
// `pir js` runs scripts on the embedded QuickJS runtime, so the whole module
// (and its `include_str!` of the script boot overlay) is an `extensions`-only
// surface. `run_mode` reports the missing feature at runtime instead.
#[cfg(feature = "extensions")]
#[path = "main_cli/js_mode.rs"]
mod js_mode;
#[path = "main_cli/print_mode.rs"]
mod print_mode;
#[path = "main_cli/run_mode.rs"]
mod run_mode;
#[path = "main_cli/tui_trace.rs"]
mod tui_trace;

// Internal cross-module helpers (private to this crate root tree).
use cli_commands::{run_export, run_list_models};
use cli_preprocess::{
    resolve_extension_paths, resolve_project_trust_for_run, resolve_run_session_plan,
    resolve_session_dir,
};
#[cfg(feature = "tui")]
use print_mode::resolve_interactive_tool_selection;
use print_mode::run_print_mode;
use run_mode::run;
use tui_trace::{rotate_tui_trace_log, validate_tui_mode, TUI_TRACE_LOG_MAX_BYTES};

/// The runtime error for a command whose subsystem was compiled out of this
/// build. `cargo` features are additive and the clap surface is shared by
/// every profile, so the command must parse and then fail with the exact
/// missing feature instead of failing to compile.
#[allow(dead_code)] // a `full` build compiles every call site out
fn missing_feature(command: &str, feature: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "`{command}` is unavailable: this build was built without the `{feature}` feature"
    )
}

/// Windows gives the main thread a 1 MiB stack, and the clap command tree
/// (`Cli`'s 60+ flags plus the subcommand trees) overflows it in debug
/// builds, where unoptimized stack frames are much larger. Run the whole
/// program on a thread with a generous stack instead of on the main thread.
/// The budget is shared with `pir js`'s script thread
/// (`env_daemon::stack_budget_bytes`), so the two deeply-recursive entry
/// points cannot drift apart.
fn main() -> Result<()> {
    let worker = std::thread::Builder::new()
        .name("pir-main".to_owned())
        .stack_size(pir::env_daemon::stack_budget_bytes(cfg!(debug_assertions)))
        .spawn(run_on_worker)?;
    match worker.join() {
        Ok(result) => result,
        // The panic hook already reported the panic on the worker thread;
        // resume it here so the process exits with the panic status.
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// The program body on the big-stack worker thread: the `#[tokio::main]`
/// default runtime, driven by the worker thread itself.
///
/// The runtime is capped for small devices: worker count is tokio's own
/// one-per-core default unless `PIR_TOKIO_WORKERS` overrides it, and the
/// blocking pool (tokio's default 512 threads x 2 MiB of reserved stack) is
/// sized from the core count with `PIR_TOKIO_MAX_BLOCKING` as the override.
fn run_on_worker() -> Result<()> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if let Some(workers) = pir::env_daemon::tokio_worker_count() {
        builder.worker_threads(workers);
    }
    builder.max_blocking_threads(pir::env_daemon::tokio_max_blocking_threads());
    builder.build()?.block_on(run())
}
