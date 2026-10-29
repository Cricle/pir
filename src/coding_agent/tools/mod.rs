//! Core coding-agent tools: read, bash, powershell, edit, write, grep, find,
//! ls, plus the model-facing background-call controls (`background_status`,
//! `background_abort`).
//!
//! Every built-in tool is its own leaf feature (`tool-read`, `tool-bash`,
//! `tool-powershell`, `tool-edit`, `tool-write`, `tool-ls`,
//! `tool-background`, plus the independent `grep-find` pair); the aggregate
//! `tools` feature is a meta-feature over the seven `tool-*` leaves. With a
//! leaf off its module and registration disappear while
//! this module keeps only what the core needs: the bash shell resolution and
//! the shell pool ([`bash`], [`shell_pool`]) that `agent_session::bash` and
//! the provider config-value executor call, the `@file` image sniffer used by
//! the print host ([`image`]), the spill file helpers used by the core bash
//! capture, and [`register_core_tools`] registering exactly the enabled
//! leaves. `confine`/`edit`/`util` also compile under `tui` alone for the
//! TUI's live edit preview.

#[cfg(feature = "tool-background")]
mod background;
mod bash;
// Handle-relative (descriptor-anchored) resolution shared by the file tools
// (`read`, `write`, `edit`). The TUI's edit preview (`compute_edits_diff`)
// reads through the same confinement, so this survives a toolless `tui` build.
#[cfg(any(
    feature = "tool-read",
    feature = "tool-write",
    feature = "tool-edit",
    feature = "tui"
))]
mod confine;
// `edit` is both the tool (`tool-edit`) and the TUI preview engine (`tui`).
#[cfg(any(feature = "tool-edit", feature = "tui"))]
mod edit;
// The ripgrep-engine pair (`grep-find`): with the feature off neither tool
// exists, exactly like a compiled-out builtin. `grep-find` is an independent
// leaf (it does NOT imply `tools` or any `tool-*` leaf); the deferred
// `tool-grep`/`tool-find` split keeps the two names under one feature.
#[cfg(feature = "grep-find")]
mod find;
#[cfg(feature = "grep-find")]
mod grep;
// Shared in-process ripgrep search core: the `grep` tool and the standalone
// `pir grep` command run the same walk/match loop (no duplicated engine).
#[cfg(feature = "grep-find")]
pub(crate) mod grep_engine;
/// Test seam for the shared grep byte budget (integration tests live in
/// `tests/`; inline test modules are forbidden in `src/`).
#[doc(hidden)]
#[cfg(feature = "grep-find")]
pub use grep_engine::search_budget_for_tests;
// `pub(crate)`: the print host sniffs `@file` image attachments with the same
// detector the read tool uses (single source of truth for the format set).
// Only those two consumers exist, so a build with neither `print` nor
// `tool-read` (a minimal SDK embed) compiles no image module at all — instead
// of compiling the sniffer's helpers as dead code.
#[cfg(any(feature = "print", feature = "tool-read"))]
pub(crate) mod image;
#[cfg(feature = "tool-ls")]
mod ls;
#[cfg(feature = "tool-powershell")]
mod powershell;
#[cfg(feature = "tool-read")]
mod read;
// `pub`: the persistent shell pool exposes its member-spawn seam to the
// integration tests (tests/shell_pool.rs). Core: the out-of-loop
// `AgentSession::execute_bash` path reuses it without the tools feature.
pub mod shell_pool;
// Restricted (owner-only) shell-output spill files with an expiry sweep.
// Core: `ShellCapture` (the rolling bash capture shared by the shell pool and
// the session's out-of-loop bash path) is compiled without `tools`, and its
// eviction/full-output behavior calls into this module.
// `pub` (hidden): the security regression tests exercise creation,
// permissions and the bounded sweep directly instead of through a shell run.
#[doc(hidden)]
pub mod spill;
// Shared workspace/path/remote-write helpers: used by every file tool (and by
// the ripgrep pair and the TUI preview), so it compiles for whichever of them
// is enabled.
#[cfg(any(
    feature = "tool-read",
    feature = "tool-write",
    feature = "tool-edit",
    feature = "tool-ls",
    feature = "grep-find",
    feature = "tui"
))]
mod util;
#[cfg(feature = "tool-write")]
mod write;

use crate::agent::tool_execution::ToolRegistry;
use crate::ai::types::Tool;

#[cfg(feature = "tool-background")]
use background::{BackgroundAbortTool, BackgroundStatusTool};

// Shell resolution backing the bash tool (`settings.shellPath`,
// `settings.shellCommandPrefix`); public so the shell-selection rules stay
// directly testable (integration tests live in `tests/`). Core: the
// `agent_session` bash path and the provider config-value executor resolve the
// same shell without the tools feature.
pub use bash::{apply_command_prefix, resolve_shell_config, ShellConfig};
// The edit preview seam (`computeEditsDiff`): the TUI computes the live edit
// diff off-thread with the same engine that later applies the edit.
// The diff preview is read by the TUI's edit renderer only.
#[cfg(feature = "tui")]
pub(crate) use edit::compute_edits_diff;

/// Test seam: whether a `ConfinedTarget::open` failure is the handle walk's own
/// confinement refusal rather than a real OS denial (`EACCES` / Windows
/// access-denied). Integration tests live in `tests/`; this seam exists so the
/// policy-vs-OS split stays directly testable without inline tests in `src/`.
/// Compiled with any file tool or the TUI edit preview (the users of
/// `confine`).
#[doc(hidden)]
#[cfg(any(
    feature = "tool-read",
    feature = "tool-write",
    feature = "tool-edit",
    feature = "tui"
))]
pub fn is_confinement_refusal_for_tests(error: &std::io::Error) -> bool {
    confine::is_confinement_refusal(error)
}

/// Test seam: the model-facing text for a `ConfinedTarget::open` failure
/// (confinement wording for a policy refusal, permission-denied wording for an
/// OS denial). The wording lives in `confine` because `read`, `write` and
/// `edit` all use it.
#[doc(hidden)]
#[cfg(any(
    feature = "tool-read",
    feature = "tool-write",
    feature = "tool-edit",
    feature = "tui"
))]
pub fn confinement_error_for_tests(requested: &str, error: &std::io::Error) -> String {
    confine::confinement_error(requested, error)
}

/// The enabled core tool name set: everything `register_core_tools` registers,
/// in the same order (upstream `allToolNames`, tools/index.ts:96). This — not
/// `DEFAULT_TOOL_NAMES` — is the built-in set the `--no-builtin-tools` filter
/// drops and the name set a full-registry base needs.
///
/// The list is assembled from the per-tool leaf features, so a partial build
/// exposes exactly the tools it compiled. The `grep`/`find` pair follows the
/// independent `grep-find` leaf (it no longer implies `tools`); its per-tool
/// split is a later tranche. With every leaf (and `grep-find`) off the slice
/// is empty.
pub const CORE_TOOL_NAMES: &[&str] = &[
    #[cfg(feature = "tool-read")]
    "read",
    #[cfg(feature = "tool-bash")]
    "bash",
    #[cfg(feature = "tool-powershell")]
    "powershell",
    #[cfg(feature = "tool-edit")]
    "edit",
    #[cfg(feature = "tool-write")]
    "write",
    #[cfg(feature = "grep-find")]
    "grep",
    #[cfg(feature = "grep-find")]
    "find",
    #[cfg(feature = "tool-ls")]
    "ls",
    #[cfg(feature = "tool-background")]
    "background_status",
    #[cfg(feature = "tool-background")]
    "background_abort",
];

// Pluggable per-tool backends (upstream `*Operations` options): a host can
// build a tool with `with_operations(..)` and replace its filesystem / shell /
// search backend (remote execution, a container, a test double). The default
// constructors (`new()`) keep today's local behavior and the `env_client`
// remote selection.
#[cfg(feature = "tool-bash")]
pub use bash::{BashExecOptions, BashOperations, BashTool};
#[cfg(feature = "tool-edit")]
pub use edit::{EditOperations, EditTool};
#[cfg(feature = "grep-find")]
pub use find::{FindGlobOptions, FindOperations, FindTool};
#[cfg(feature = "grep-find")]
pub use grep::{GrepOperations, GrepTool};
#[cfg(feature = "tool-ls")]
pub use ls::{LsOperations, LsTool};
#[cfg(feature = "tool-powershell")]
pub use powershell::{PowerShellOperations, PowerShellTool};
#[cfg(feature = "tool-read")]
pub use read::{ReadOperations, ReadTool};
#[cfg(feature = "tool-write")]
pub use write::{WriteOperations, WriteTool};

/// Register every enabled core coding-agent tool into a registry.
///
/// Every enabled core tool is REGISTERED (upstream `createAllToolDefinitions`
/// registers its whole set unconditionally; `powershell` execution resolves no
/// executable off-Windows), but registration is not activation: `powershell`
/// carries `defaultActive: false` (it is absent from upstream
/// `DEFAULT_TOOL_NAMES`), so the registration-active base leaves it undeclared.
/// Hosts that resolve a tool selection keep tools out via the selection itself,
/// not via a registration-time gate. Without the `grep-find` feature the
/// `grep`/`find` pair is not registered at all; the background controls are
/// registered whenever `tool-background` is on (they answer "no registry" on a
/// host that runs none).
///
/// With no leaf feature enabled the function is a no-op: the build has no
/// built-in tools, and this keeps every host (`print`, `rpc`, the TUI, the
/// server, the SDK builder) compiling unchanged.
pub fn register_core_tools(registry: &mut ToolRegistry) {
    let _ = &registry;
    #[cfg(feature = "tool-read")]
    registry.register(std::sync::Arc::new(ReadTool::new()));
    #[cfg(feature = "tool-bash")]
    registry.register(std::sync::Arc::new(BashTool::new()));
    #[cfg(feature = "tool-powershell")]
    registry.register(std::sync::Arc::new(PowerShellTool::new()));
    #[cfg(feature = "tool-write")]
    registry.register(std::sync::Arc::new(WriteTool::new()));
    #[cfg(feature = "tool-edit")]
    registry.register(std::sync::Arc::new(EditTool::new()));
    #[cfg(feature = "grep-find")]
    registry.register(std::sync::Arc::new(GrepTool::new()));
    #[cfg(feature = "grep-find")]
    registry.register(std::sync::Arc::new(FindTool::new()));
    #[cfg(feature = "tool-ls")]
    registry.register(std::sync::Arc::new(LsTool::new()));
    // Model-facing background-call controls: ordinary direct-exposure tools
    // (default-active like every core tool). They report "no registry" on a
    // host that runs none, so registering them is safe.
    #[cfg(feature = "tool-background")]
    registry.register(std::sync::Arc::new(BackgroundStatusTool));
    #[cfg(feature = "tool-background")]
    registry.register(std::sync::Arc::new(BackgroundAbortTool));
}

/// Convert registered tools into the model-facing JSON tool definitions.
///
/// Keeping this conversion next to the core tool registration ensures every
/// caller uses the same descriptions and schemas for execution and provider
/// requests.
pub fn tool_definitions(registry: &ToolRegistry) -> Vec<Tool> {
    let mut definitions = registry
        .iter()
        .map(|tool| Tool {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters: tool
                .parameters()
                .unwrap_or_else(|| serde_json::json!({"type": "object"})),
            constrained_sampling: tool.constrained_sampling(),
        })
        .collect::<Vec<_>>();
    definitions.sort_by(|left, right| left.name.cmp(&right.name));
    definitions
}
