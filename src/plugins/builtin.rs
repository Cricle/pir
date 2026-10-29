//! The Rust builtin-plugin registration point.
//!
//! This mirrors upstream `packages/coding-agent/src/extensions/index.ts`:
//!
//! ```ts
//! export const builtInExtensions: InlineExtension[] = [
//!   { name: "llama.cpp", factory: llamaExtension, builtin: true },
//!   { name: "codemode", factory: codemodeExtension, replaceable: true, builtin: true },
//!   ...
//! ];
//! ```
//!
//! ## Where a new Rust builtin gets added
//!
//! 1. Write the plugin struct in its own module (e.g. `src/plugins/subagent/`)
//!    implementing [`AgentPlugin`].
//! 2. Append a [`BuiltinExtensionDef`] to [`BUILTIN_EXTENSION_FACTORIES`]
//!    below with its `name` (addressed as `builtin:<name>` from `-e
//!    builtin:<name>` / `disabledBuiltinExtensions`), `replaceable` flag, and
//!    a factory returning `Arc<dyn AgentPlugin>`.
//! 3. Nothing else: [`PluginHost::build`] walks the `-e` path plan, resolves
//!    `builtin:<name>` entries through this list (unknown names produce the
//!    upstream `Unknown built-in extension:` error), registers the plugin
//!    into the shared [`PluginRegistry`], and dispatches its events on the
//!    unified ordered list.
//!
//! Upstream keeps the list small; pir's builtins are `subagent`, `mcp`, and
//! `codemode` — the same set upstream ships (minus its JS-only entries) —
//! pir-only `python` (the embedded RustPython 3 interpreter) and `tool-search`
//! (upstream extensions/tool-search), plus `tool-repair` (the
//! pi-tool-repair npm package ported natively).

use std::sync::Arc;

use super::plugin::AgentPlugin;
use super::source::BUILTIN_PATH_PREFIX;

/// One entry of the builtin factory list (upstream `InlineExtension` with
/// `builtin: true`).
pub struct BuiltinExtensionDef {
    /// The `builtin:<name>` name.
    pub name: &'static str,
    /// Whether a non-replaceable plugin registering the same
    /// tool/command/flag name replaces (omits) this builtin.
    pub replaceable: bool,
    /// Factory producing the plugin instance (upstream `factory`).
    pub factory: fn() -> Arc<dyn AgentPlugin>,
    /// Whether `default_builtin_paths` puts this builtin on the plan when no
    /// explicit `builtin:<name>`/settings entry names it. Existing builtins are
    /// on by default; `phone` is off (it binds a local port and holds a server
    /// task until `phone-start`).
    pub default_enabled: bool,
}

/// The builtin factory list, upstream
/// `extensionFactories = [...builtInExtensions, ...]`.
///
/// >>> ADD NEW RUST BUILTINS HERE <<<
pub const BUILTIN_EXTENSION_FACTORIES: &[BuiltinExtensionDef] = &[
    // pi's subagent, ported natively (upstream
    // packages/coding-agent/examples/extensions/subagent) — the first
    // JS-extension-to-Rust-builtin conversion. Replaceable: a user JS
    // extension registering a `subagent` tool omits this builtin. Absent in a
    // build without the `subagent` feature, so `builtin:subagent` reports the
    // unknown-builtin error there.
    #[cfg(feature = "subagent")]
    BuiltinExtensionDef {
        name: "subagent",
        replaceable: true,
        factory: new_subagent_plugin,
        default_enabled: true,
    },
    // The native MCP client integration (upstream
    // packages/coding-agent/src/extensions/mcp): server lifecycle, tool
    // exposure (`mcp__<server>__<tool>`), `mcp_servers_change` events.
    // Replaceable: a user JS extension registering an `mcp`-named tool omits
    // it; `--no-mcp` disables it outright. Absent in a build without the `mcp`
    // feature, so `builtin:mcp` reports the unknown-builtin error there.
    #[cfg(feature = "mcp")]
    BuiltinExtensionDef {
        name: "mcp",
        replaceable: true,
        factory: new_mcp_plugin,
        default_enabled: true,
    },
    // pi's codemode tool-composition sandbox (upstream
    // packages/coding-agent/src/extensions/codemode), ported natively: a
    // minimal QuickJS runtime per call whose only capability is the
    // nested-tool-call bridge. Replaceable: a user JS extension registering a
    // `codemode` tool omits this builtin. Absent in a build without the
    // `extensions` feature (no embedded QuickJS engine), so `builtin:codemode`
    // reports the unknown-builtin error there.
    #[cfg(feature = "extensions")]
    BuiltinExtensionDef {
        name: "codemode",
        replaceable: true,
        factory: new_codemode_plugin,
        default_enabled: true,
    },
    // pir-only: the embedded RustPython 3 `python` tool (no upstream
    // counterpart — upstream reaches Python by spawning the user's native
    // `python3` through bash). Persistent per-session VM, frozen pure-Python
    // stdlib, host-side deadline interrupts. Replaceable: a user extension
    // registering a `python` tool omits this builtin; the `pythonTool`
    // setting (via `PythonPlugin::with_config`) or
    // `disabledBuiltinExtensions: ["python"]` disables it. Absent in a build
    // without the `python` feature, so `builtin:python` reports the
    // unknown-builtin error there.
    #[cfg(feature = "python")]
    BuiltinExtensionDef {
        name: "python",
        replaceable: true,
        factory: new_python_plugin,
        default_enabled: true,
    },
    // The native `context-mode` port: "think in code" execution
    // (`ctx_execute` / `ctx_execute_file`: only stdout enters context) plus an
    // FTS5/BM25 knowledge base (`ctx_index` / `ctx_search` /
    // `ctx_fetch_and_index` / `ctx_stats` / `ctx_purge`). Reuses the embedded
    // RustPython worker and the bundled SQLite. Default-enabled but lazy: no
    // interpreter thread and no DB handle until the first execute/index call.
    // Replaceable: a user extension registering any of the same tool names
    // omits it. Absent in a build without the `context-mode` feature, so
    // `builtin:context-mode` reports the unknown-builtin error there.
    #[cfg(feature = "context-mode")]
    BuiltinExtensionDef {
        name: "context-mode",
        replaceable: true,
        factory: new_context_mode_plugin,
        default_enabled: true,
    },
    // The BM25 tool_search builtin (upstream extensions/tool-search):
    // searches the NOT-yet-active codemode/deferred tools and loads the
    // matches (`defaultActive: false`; activated via `--tools` /
    // `defaultTools` / the mcp deferred-server auto-activation upstream).
    // Replaceable: a user JS extension registering a `tool_search` tool omits
    // this builtin. Absent in a build without the `tool-search` feature, so
    // `builtin:tool-search` reports the unknown-builtin error there (the
    // [`TOOL_SEARCH_TOOL_NAME`] constant stays available: the MCP reach layer
    // recognizes a `tool_search`-named tool whether or not this builtin is
    // compiled in).
    #[cfg(feature = "tool-search")]
    BuiltinExtensionDef {
        name: "tool-search",
        replaceable: true,
        factory: new_tool_search_plugin,
        default_enabled: true,
    },
    // pi-tool-repair (monotykamary/pi-tool-repair 0.3.6), ported natively:
    // the assistant `message_end` repair pipeline (GLM token-leak strip,
    // anchor-bleed strip, grammar-leak recovery, live-schema repair, phantom
    // toolUse normalization) applied through the loop's native
    // `message_end_repair` seam (agent_session/tool_repair.rs). Registers no
    // tool/command/flag, so `replaceable` is the registration channel only.
    // Disabled via `disabledBuiltinExtensions: ["tool-repair"]`. With the JS
    // `pi-tool-repair` package also on the path plan the NATIVE builtin wins:
    // the loader drops the JS plan entry (`loader::apply_tool_repair_precedence`)
    // so the same repair pipeline does not run twice (the JS face is the same
    // package; the loop applies generic JS `message_end` returns since the
    // parity lane, so the JS `pi-tool-repair` message_end repair lands fine
    // when it is the chosen face); the JS copy is loaded only when this
    // builtin is disabled.
    // Absent in a build without the `tool-repair` feature, so
    // `builtin:tool-repair` reports the unknown-builtin error there (the JS
    // `pi-tool-repair` fallback then stays on the plan — the same shape as a
    // `disabledBuiltinExtensions` omission).
    #[cfg(feature = "tool-repair")]
    BuiltinExtensionDef {
        name: "tool-repair",
        replaceable: true,
        factory: new_tool_repair_plugin,
        default_enabled: true,
    },
    // The npm `@juicesharp/rpiv-todo` 2.12.0 package ported natively: the
    // model-facing `todo` task list (six actions, the status machine,
    // blockedBy edges, tombstones, the response envelope) plus the `/todos`
    // command. State is rebuilt from the session transcript on every
    // `on_session_context` push whose entries changed, so the list survives
    // `/reload`, `/resume`, and compaction. Replaceable: a user JS extension
    // registering a `todo` tool / `todos` command omits this builtin. Absent
    // in a build without the `todo` feature, so `builtin:todo` reports the
    // unknown-builtin error there. v1 scope (no TUI panel, no i18n, no
    // settings): `docs/todo-builtin.md`.
    #[cfg(feature = "todo")]
    BuiltinExtensionDef {
        name: "todo",
        replaceable: true,
        factory: new_todo_plugin,
        default_enabled: true,
    },
    // pi-phone, ported natively: the phone-first remote UI (local HTTP+WS
    // server, parent-session mirror, parallel RPC sessions, Tailscale).
    // DEFAULT OFF: it binds a local port and holds a server task, so it joins
    // the default plan only when named explicitly (`builtin:phone` in settings
    // or `-e`); the JS `@malinamnam/pi-phone` package is default-disabled too.
    #[cfg(feature = "phone")]
    BuiltinExtensionDef {
        name: "phone",
        replaceable: true,
        factory: new_phone_plugin,
        default_enabled: false,
    },
];

/// Names of every registered builtin.
pub fn builtin_names() -> Vec<&'static str> {
    BUILTIN_EXTENSION_FACTORIES
        .iter()
        .map(|def| def.name)
        .collect()
}

/// Look up a builtin factory by `builtin:<name>` name.
pub fn builtin_extension_def(name: &str) -> Option<&'static BuiltinExtensionDef> {
    BUILTIN_EXTENSION_FACTORIES
        .iter()
        .find(|def| def.name == name)
}

/// Every builtin name together with the cargo feature that compiles it in.
/// Used only for the diagnostics of a `builtin:<name>` path that did not
/// resolve: in a full build every name here is registered, so the entry is
/// reachable only from a trimmed build.
const BUILTIN_FEATURES: &[(&str, &str)] = &[
    ("subagent", "subagent"),
    ("tool-repair", "tool-repair"),
    ("tool-search", "tool-search"),
    ("mcp", "mcp"),
    ("python", "python"),
    ("context-mode", "context-mode"),
    ("codemode", "extensions"),
    ("todo", "todo"),
    ("phone", "phone"),
];

/// Diagnostic for a `builtin:<name>` pseudo-path that did not resolve.
///
/// A name that is a real builtin in a full build but is absent here names the
/// missing feature, so a trimmed build says *why* instead of claiming the
/// name is unknown; a genuinely unknown name keeps the upstream wording
/// (`Unknown built-in extension: builtin:<name>`).
pub fn missing_builtin_message(name: &str) -> String {
    match BUILTIN_FEATURES
        .iter()
        .find(|(builtin, _)| *builtin == name)
    {
        Some((_, feature)) => format!(
            "Built-in extension `{BUILTIN_PATH_PREFIX}{name}` is not compiled into this build \
             (the `{feature}` feature is off)"
        ),
        None => format!("Unknown built-in extension: {BUILTIN_PATH_PREFIX}{name}"),
    }
}

/// Instantiate the builtin plugin for `builtin:<name>`; `None` for an
/// unknown name (the caller reports the upstream
/// `Unknown built-in extension:` error).
pub fn instantiate_builtin(name: &str) -> Option<Arc<dyn AgentPlugin>> {
    builtin_extension_def(name).map(|def| (def.factory)())
}

// ---------------------------------------------------------------------------
// Builtin factories
// ---------------------------------------------------------------------------

#[cfg(feature = "subagent")]
fn new_subagent_plugin() -> Arc<dyn AgentPlugin> {
    super::subagent::new_subagent_plugin()
}

#[cfg(feature = "mcp")]
pub(crate) mod mcp;

/// Public re-exports of the `mcp` builtin's integration surface (tests and
/// the tool-selection layer).
#[cfg(feature = "mcp")]
pub use mcp::{
    configured_exposures, convert_mcp_result, mcp_server_namespace, mcp_tool_owner, reaches_server,
    reaches_server_by_alias, resource_exposure, DiscoveryActivation, McpPlugin, McpResourceTool,
    McpResourceToolKind, McpToolBridge,
};

#[cfg(feature = "mcp")]
fn new_mcp_plugin() -> Arc<dyn AgentPlugin> {
    mcp::new_mcp_plugin()
}

#[cfg(feature = "extensions")]
fn new_codemode_plugin() -> Arc<dyn AgentPlugin> {
    super::codemode::new_codemode_plugin()
}

/// Public re-export of the `codemode` builtin's tool name, reach rule, and
/// namespace lookup (upstream `CODEMODE_TOOL_NAME` / `getScriptReach` /
/// `isNamespaceName`; the MCP side looks the tool up by name).
#[cfg(feature = "extensions")]
pub use super::codemode::{namespace_matches, script_reach, CODEMODE_TOOL_NAME};

#[cfg(feature = "python")]
pub(crate) mod python;

#[cfg(feature = "python")]
pub(crate) mod python_vm;

#[cfg(feature = "context-mode")]
pub(crate) mod context_mode;

/// The registered `tool_search` tool name (upstream `TOOL_SEARCH_TOOL_NAME`).
///
/// Defined here, NOT in the gated `tool_search` module: the MCP reach layer
/// (`builtin::mcp`'s discovery activation and `agent_session::mcp_reach`)
/// recognizes a tool registered under this name — the builtin's or a user JS
/// extension's — in every build, with or without the `tool-search` feature.
pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

#[cfg(feature = "tool-search")]
pub(crate) mod tool_search;

/// Public re-export of the `tool-search` builtin (tests and embedders).
/// [`Bm25Ranker`](tool_search::Bm25Ranker) rides along for the
/// ordering-determinism tests (the documented score-desc/name-asc key).
#[cfg(feature = "tool-search")]
pub use tool_search::{Bm25Ranker, ToolSearchPlugin};

#[cfg(feature = "tool-search")]
fn new_tool_search_plugin() -> Arc<dyn AgentPlugin> {
    Arc::new(tool_search::ToolSearchPlugin::new())
}

#[cfg(feature = "tool-repair")]
fn new_tool_repair_plugin() -> Arc<dyn AgentPlugin> {
    super::tool_repair::new_tool_repair_plugin()
}

#[cfg(feature = "todo")]
pub mod todo;

#[cfg(feature = "phone")]
pub mod phone;

#[cfg(feature = "phone")]
fn new_phone_plugin() -> Arc<dyn AgentPlugin> {
    phone::new_phone_plugin()
}

/// Public re-exports of the `todo` builtin (tests, and hosts that want the
/// live task state). `new_todo_plugin` is also the factory the builtin table
/// above names; the module itself is public for the full ported surface.
#[cfg(feature = "todo")]
pub use todo::{
    new_todo_plugin, TodoPlugin, TodoState, COMMAND_NAME as TODO_COMMAND_NAME,
    TOOL_NAME as TODO_TOOL_NAME,
};

/// Public re-exports of the `python` builtin's integration surface (tests,
/// embedders that resolve the `pythonTool` setting, and the tool-selection
/// layer).
#[cfg(feature = "python")]
pub use python::{PythonPlugin, PythonTool, MAX_OUTPUT_CHARS, PYTHON_TOOL_NAME};

#[cfg(feature = "python")]
fn new_python_plugin() -> Arc<dyn AgentPlugin> {
    python::PythonPlugin::new_plugin()
}

/// Public re-exports of the `context-mode` builtin's integration surface
/// (tests, embedders, the tool-selection layer).
#[cfg(feature = "context-mode")]
pub use context_mode::{
    ContextModePlugin, CTX_BATCH_EXECUTE_TOOL_NAME, CTX_DOCTOR_TOOL_NAME,
    CTX_EXECUTE_FILE_TOOL_NAME, CTX_EXECUTE_TOOL_NAME, CTX_FETCH_AND_INDEX_TOOL_NAME,
    CTX_INDEX_TOOL_NAME, CTX_PURGE_TOOL_NAME, CTX_SEARCH_TOOL_NAME, CTX_STATS_TOOL_NAME,
};

#[cfg(feature = "context-mode")]
fn new_context_mode_plugin() -> Arc<dyn AgentPlugin> {
    context_mode::ContextModePlugin::new_plugin()
}
