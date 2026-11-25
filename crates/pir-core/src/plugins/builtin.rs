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
//! Upstream keeps the list small; pir's builtins are `subagent`, `mode` (the
//! user-triggered `/mode foreman` / `/mode plan` session modes), `mcp`, and
//! `codemode` — the same set upstream ships (minus its JS-only entries) plus
//! the mode command, `context-mode` (the Code Mode execution surface ported
//! natively and run through the embedded JS suite), `tool-search` (upstream
//! extensions/tool-search), `tool-repair` (the pi-tool-repair npm package
//! ported natively), `todo` (the rpiv-todo port), and `memory` (the embedded
//! cm-memory engine; always compiled with the `memory` feature and
//! default-enabled).

use std::path::Path;
use std::sync::{Arc, OnceLock};

use super::plugin::AgentPlugin;
use super::source::BUILTIN_PATH_PREFIX;
use crate::extensions::ExtensionHandle;

/// The per-session inputs a [`BuiltinRewire`] hook re-roots a builtin with.
///
/// A named struct rather than positional arguments on purpose: the two paths
/// were previously passed as `(Option<&Path>, &Path)`, and both in-core hooks
/// read them transposed, so `mcp` and `memory` were re-rooted with the agent
/// directory as the session cwd. Field names make that class of bug
/// unrepresentable.
pub struct BuiltinSessionPaths<'a> {
    /// The agent directory (`<agent_dir>`), when the host has one. `None`
    /// means the host keeps the plugin's process-global default.
    pub agent_dir: Option<&'a Path>,
    /// The session working directory.
    pub cwd: &'a Path,
    /// The JS runtime handle, when this session has one.
    pub js: Option<ExtensionHandle>,
}

impl<'a> BuiltinSessionPaths<'a> {
    pub fn new(agent_dir: Option<&'a Path>, cwd: &'a Path, js: Option<ExtensionHandle>) -> Self {
        Self { agent_dir, cwd, js }
    }

    /// The agent directory this hook must use: the explicit one, else the
    /// process default.
    pub fn resolved_agent_dir(&self) -> std::path::PathBuf {
        self.agent_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(crate::coding_agent::context::agent_dir)
    }
}

/// Re-root or attach a builtin instance for one session before registration.
///
/// The hook is the extraction seam for standalone builtin crates: the core
/// host knows only this function pointer and never downcasts a concrete plugin.
pub type BuiltinRewire = fn(Arc<dyn AgentPlugin>, &BuiltinSessionPaths<'_>) -> Arc<dyn AgentPlugin>;

/// Resolves a builtin name to a plugin instance for a catalog that supplies
/// its implementations from outside `pir-core`.
///
/// The resolver is consulted before the catalog's static [`entries`]:
/// an application layer can therefore own the concrete factories (and any
/// per-process state they need) while core keeps only the protocol and the
/// host. [`BuiltinCatalog::entries`] still decides which names exist for
/// discovery/validation.
pub type BuiltinResolver = dyn Fn(&str) -> Option<Arc<dyn AgentPlugin>> + Send + Sync + 'static;

/// A catalog of builtin factories supplied by an application layer.
///
/// `pir-core` keeps the protocol and host, while an aggregate crate owns the
/// concrete implementations. The legacy in-core table remains the default
/// catalog during the staged migration: when no catalog is installed
/// ([`install_builtin_catalog`]), every catalog-aware entry point falls back
/// to [`BUILTIN_EXTENSION_FACTORIES`].
#[derive(Clone, Copy)]
pub struct BuiltinCatalog {
    pub entries: &'static [BuiltinExtensionDef],
    /// Optional external factory resolver; `None` for the legacy table.
    resolver: Option<&'static BuiltinResolver>,
}

impl BuiltinCatalog {
    pub const fn new(entries: &'static [BuiltinExtensionDef]) -> Self {
        Self {
            entries,
            resolver: None,
        }
    }

    /// A catalog whose instances come from `resolver` (consulted first), with
    /// `entries` still describing the names/defaults the loader may use.
    pub const fn with_resolver(
        entries: &'static [BuiltinExtensionDef],
        resolver: &'static BuiltinResolver,
    ) -> Self {
        Self {
            entries,
            resolver: Some(resolver),
        }
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.entries.iter().map(|entry| entry.name).collect()
    }

    pub fn get(&self, name: &str) -> Option<&BuiltinExtensionDef> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// Whether this catalog declares `name` (the loader's unknown-name check).
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Instantiate `name`: the external resolver first, then the entry's
    /// factory. `None` means this catalog does not provide the builtin.
    pub fn resolve(&self, name: &str) -> Option<Arc<dyn AgentPlugin>> {
        if let Some(resolver) = self.resolver {
            if let Some(plugin) = resolver(name) {
                return Some(plugin);
            }
        }
        self.get(name).map(|def| (def.factory)())
    }
}

/// The application-installed catalog, if any.
static INSTALLED_CATALOG: OnceLock<BuiltinCatalog> = OnceLock::new();

/// Install the application-layer builtin catalog.
///
/// Returns `Err(existing)` when a catalog was already installed: the first
/// install wins, so a host cannot be silently re-pointed mid-process. The
/// legacy in-core table stays in use until this succeeds.
pub fn install_builtin_catalog(catalog: BuiltinCatalog) -> Result<(), BuiltinCatalog> {
    INSTALLED_CATALOG.set(catalog)
}

/// The installed application catalog, if one was installed.
pub fn installed_builtin_catalog() -> Option<BuiltinCatalog> {
    INSTALLED_CATALOG.get().copied()
}

/// The catalog catalog-aware core entry points consult by default: the
/// installed catalog, else the legacy in-core table.
pub fn active_builtin_catalog() -> BuiltinCatalog {
    installed_builtin_catalog().unwrap_or_else(builtin_catalog)
}

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
    /// Optional extraction hook for session-specific paths and runtime attach.
    pub rewire: Option<BuiltinRewire>,
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
        rewire: Some(rewire_subagent),
        default_enabled: true,
    },
    // User-triggered session modes. `foreman` makes the main session a
    // read-only router that delegates mutations through the existing
    // `subagent` tool; `plan` is read-only investigation/planning; `normal`
    // restores the baseline.
    #[cfg(feature = "subagent")]
    BuiltinExtensionDef {
        name: "mode",
        replaceable: true,
        factory: new_mode_plugin,
        rewire: None,
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
        rewire: Some(rewire_mcp),
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
        rewire: None,
        default_enabled: true,
    },
    // pir-only: the `python` tool (no upstream counterpart — upstream reaches
    // Python by spawning the user's native `python3` through bash). The name is
    // kept for Settings/tool-selection compatibility; the interpreter behind it
    // is the embedded QuickJS runtime (`js_vm`), so model/script code is
    // JavaScript. Persistent per-session runtime, host-side deadline
    // interrupts. Replaceable: a user extension registering a `python` tool
    // omits this builtin; the `pythonTool` setting (via
    // `PythonPlugin::with_config`) or `disabledBuiltinExtensions: ["python"]`
    // disables it. Absent in a build without the `python` feature, so
    // `builtin:python` reports the unknown-builtin error there.
    #[cfg(feature = "python")]
    BuiltinExtensionDef {
        name: "python",
        replaceable: true,
        factory: new_python_plugin,
        rewire: None,
        default_enabled: true,
    },
    // The native `context-mode` port: "think in code" execution
    // (`ctx_execute` / `ctx_execute_file`: only stdout enters context) plus an
    // FTS5/BM25 knowledge base (`ctx_index` / `ctx_search` /
    // `ctx_fetch_and_index` / `ctx_stats` / `ctx_purge`), the upstream shell
    // batch (`ctx_batch`) and the 2,048-byte journal block (`ctx_resume`).
    // Execution reuses the embedded QuickJS runtime and its node compatibility
    // suite (the same engine `pir js` and JS extensions use); the index uses
    // the bundled SQLite. Default-enabled but lazy: no runtime and no DB handle
    // until the first execute/index call. Replaceable: a user extension
    // registering any of the same tool names omits it. Absent in a build without the
    // `context-mode` feature, so `builtin:context-mode` reports the
    // unknown-builtin error there.
    #[cfg(feature = "context-mode")]
    BuiltinExtensionDef {
        name: "context-mode",
        replaceable: true,
        factory: new_context_mode_plugin,
        rewire: None,
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
        rewire: None,
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
        rewire: None,
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
        rewire: None,
        default_enabled: true,
    },
    // The embedded `cm-memory` engine: the model-facing `memory` tool with a
    // CLI-shaped action enum, the `/memory` command, and the input auto-recall
    // hook. The store lives next to the session transcript by default (see
    // `docs/memory-builtin.md`). Replaceable: a user extension registering a
    // `memory` tool / `memory` command omits this builtin. Absent in a build
    // without the `memory` feature, so `builtin:memory` reports the
    // unknown-builtin error there.
    #[cfg(feature = "memory")]
    BuiltinExtensionDef {
        name: "memory",
        replaceable: true,
        factory: new_memory_plugin,
        rewire: Some(rewire_memory),
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
        rewire: None,
        default_enabled: false,
    },
];

/// Names of every registered builtin.
pub fn builtin_catalog() -> BuiltinCatalog {
    BuiltinCatalog::new(BUILTIN_EXTENSION_FACTORIES)
}

/// The feature-independent builtin metadata table (order, default-enable
/// flags, host feature per builtin) owned by the `pir-builtins` crate.
///
/// Re-exported here because [`BUILTIN_EXTENSION_FACTORIES`] must agree with it
/// on order and flags; `tests/builtin_catalog_agreement.rs` pins that, so the
/// two tables cannot drift while both exist.
pub use pir_builtins::{KnownBuiltin, KNOWN_BUILTINS};

pub fn builtin_names() -> Vec<&'static str> {
    active_builtin_catalog().names()
}

/// Look up a builtin factory by `builtin:<name>` name.
///
/// Consults the installed application catalog first and falls back to the
/// legacy in-core table when none was installed.
pub fn builtin_extension_def(name: &str) -> Option<&'static BuiltinExtensionDef> {
    match installed_builtin_catalog() {
        Some(catalog) => catalog.entries.iter().find(|entry| entry.name == name),
        None => BUILTIN_EXTENSION_FACTORIES
            .iter()
            .find(|entry| entry.name == name),
    }
}

/// Catalog-aware twin of [`missing_builtin_message`]: a name absent from
/// `catalog` is reported as unknown unless the legacy table knows it (a
/// trimmed legacy build), in which case the missing-feature diagnostic stays.
pub fn missing_builtin_message_in(name: &str, catalog: &BuiltinCatalog) -> String {
    if catalog.contains(name) {
        return format!(
            "Built-in extension `{BUILTIN_PATH_PREFIX}{name}` is not available in the active catalog"
        );
    }
    missing_builtin_message(name)
}

/// Diagnostic for a `builtin:<name>` pseudo-path that did not resolve.
///
/// A name that is a real builtin in a full build but is absent here names the
/// missing feature, so a trimmed build says *why* instead of claiming the
/// name is unknown; a genuinely unknown name keeps the upstream wording
/// (`Unknown built-in extension: builtin:<name>`).
///
/// The table is `pir_builtins::KNOWN_BUILTINS`, which is deliberately
/// feature-independent: this function's whole job is to describe builtins this
/// build does NOT have, so the rows cannot come from the enabled catalog.
pub fn missing_builtin_message(name: &str) -> String {
    match pir_builtins::known(name) {
        Some(row) => format!(
            "Built-in extension `{BUILTIN_PATH_PREFIX}{name}` is not compiled into this build \
             (the `{}` feature is off)",
            row.host_feature
        ),
        None => format!("Unknown built-in extension: {BUILTIN_PATH_PREFIX}{name}"),
    }
}

/// Instantiate the builtin plugin for `builtin:<name>`; `None` for an
/// unknown name (the caller reports the upstream
/// `Unknown built-in extension:` error).
pub fn instantiate_builtin(name: &str) -> Option<Arc<dyn AgentPlugin>> {
    active_builtin_catalog().resolve(name)
}

// ---------------------------------------------------------------------------
// Legacy session re-rooting hooks
// ---------------------------------------------------------------------------
//
// The two in-core builtins whose factory defaults must follow a session were
// previously re-created by name downcasts in `PluginHost::build` and in the
// SDK builder. Expressing them as catalog `rewire` hooks keeps the host
// generic: it invokes the catalog entry's hook instead of knowing the concrete
// plugin types. An application-supplied catalog can provide its own hooks (or
// none, in which case the factory instance is used unchanged).

#[cfg(feature = "subagent")]
fn rewire_subagent(
    _plugin: Arc<dyn AgentPlugin>,
    paths: &BuiltinSessionPaths<'_>,
) -> Arc<dyn AgentPlugin> {
    // The user-agent directory follows the session's agent directory: an SDK
    // agent built around `.agent_dir(..)` must discover ITS agents, and the
    // tool description must name that directory.
    Arc::new(super::subagent::SubagentPlugin::with_user_agents_dir(
        paths.resolved_agent_dir().join("agents"),
    ))
}

#[cfg(feature = "mcp")]
fn rewire_mcp(
    _plugin: Arc<dyn AgentPlugin>,
    paths: &BuiltinSessionPaths<'_>,
) -> Arc<dyn AgentPlugin> {
    // Both roots come from the session: the manager's global root (mcp.log,
    // mcp-auth.json, global mcp.json) is the agent directory, and its project
    // root follows the session cwd.
    Arc::new(mcp::McpPlugin::with_cwd(
        &paths.resolved_agent_dir(),
        paths.cwd,
    ))
}

#[cfg(feature = "memory")]
fn rewire_memory(
    _plugin: Arc<dyn AgentPlugin>,
    paths: &BuiltinSessionPaths<'_>,
) -> Arc<dyn AgentPlugin> {
    Arc::new(memory::MemoryPlugin::with_paths(
        paths.resolved_agent_dir(),
        paths.cwd,
    ))
}

// ---------------------------------------------------------------------------
// Builtin factories
// ---------------------------------------------------------------------------

#[cfg(feature = "subagent")]
fn new_subagent_plugin() -> Arc<dyn AgentPlugin> {
    super::subagent::new_subagent_plugin()
}

#[cfg(feature = "subagent")]
pub mod mode;

#[cfg(feature = "subagent")]
fn new_mode_plugin() -> Arc<dyn AgentPlugin> {
    mode::new_mode_plugin()
}

#[cfg(feature = "mcp")]
pub mod mcp;

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

#[cfg(feature = "context-mode")]
pub mod context_mode;

/// The `python` builtin's execution engine: a lazily-started QuickJS session.
///
/// Shared with the CLI's `pir python` (`native_cli::python`). Declared here,
/// next to the tool, because both the plugin and the CLI drive the same
/// session contract. Model/script code is JavaScript; the tool name is the
/// only thing left of the Python surface.
#[cfg(feature = "python")]
pub mod js_vm;

/// Public re-export of the `python` builtin's integration surface (tests and
/// embedders). The tool name and the plugin type only; the interpreter behind
/// it is the shared `js_vm` session, not a Python VM.
#[cfg(feature = "python")]
pub mod python;

#[cfg(feature = "python")]
pub use python::{PythonPlugin, PythonSession, PYTHON_TOOL_NAME};

#[cfg(feature = "python")]
fn new_python_plugin() -> Arc<dyn AgentPlugin> {
    python::PythonPlugin::new_plugin()
}

/// The registered `tool_search` tool name (upstream `TOOL_SEARCH_TOOL_NAME`).
///
/// Defined here, NOT in the gated `tool_search` module: the MCP reach layer
/// (`builtin::mcp`'s discovery activation and `agent_session::mcp_reach`)
/// recognizes a tool registered under this name — the builtin's or a user JS
/// extension's — in every build, with or without the `tool-search` feature.
pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

#[cfg(feature = "tool-search")]
pub mod tool_search;

/// Public re-export of the `tool-search` builtin (tests and embedders).
/// [`Bm25Ranker`](tool_search::Bm25Ranker) and
/// [`tokenize`](tool_search::tokenize) ride along for the text-pipeline and
/// ordering-determinism tests (the documented score-desc/name-asc key).
#[cfg(feature = "tool-search")]
pub use tool_search::{
    tokenize, tool_search_document_text, Bm25Document, Bm25Ranker, ToolSearchPlugin,
};

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

#[cfg(feature = "memory")]
pub mod memory;

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

#[cfg(feature = "memory")]
pub use memory::{
    MemoryConfig, MemoryPlugin, MemoryService, MemoryStoreMode, MemoryTool, MemoryWriteMode,
    COMMAND_NAME as MEMORY_COMMAND_NAME, TOOL_NAME as MEMORY_TOOL_NAME,
};

#[cfg(feature = "memory")]
fn new_memory_plugin() -> Arc<dyn AgentPlugin> {
    memory::new_memory_plugin()
}

/// Public re-exports of the `context-mode` builtin's integration surface
/// (tests, embedders, the tool-selection layer).
#[cfg(feature = "context-mode")]
pub use context_mode::{
    ContextModePlugin, CTX_BATCH_EXECUTE_TOOL_NAME, CTX_BATCH_TOOL_NAME, CTX_DOCTOR_TOOL_NAME,
    CTX_EXECUTE_FILE_TOOL_NAME, CTX_EXECUTE_TOOL_NAME, CTX_EXECUTE_TOOL_PROXY_NAME,
    CTX_FETCH_AND_INDEX_TOOL_NAME, CTX_INDEX_TOOL_NAME, CTX_PURGE_TOOL_NAME, CTX_RESUME_TOOL_NAME,
    CTX_SEARCH_TOOL_NAME, CTX_STATS_TOOL_NAME, MAX_OUTPUT_CHARS,
};

#[cfg(feature = "context-mode")]
fn new_context_mode_plugin() -> Arc<dyn AgentPlugin> {
    context_mode::ContextModePlugin::new_plugin()
}
