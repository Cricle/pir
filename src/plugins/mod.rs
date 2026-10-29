//! The unified plugin framework: ONE plugin logic for Rust builtin plugins
//! and QuickJS JS extensions.
//!
//! Architecture (parity with upstream pi's extension system):
//!
//! - [`plugin::AgentPlugin`] is the single plugin concept. A Rust builtin
//!   implements it natively (typed [`plugin::RegistrationContext`], no JSON
//!   round-trip); the embedded QuickJS runtime rides it as ONE adapter
//!   ([`host::JsExtensionsPlugin`]) over the unchanged `ExtensionHandle`
//!   actor.
//! - [`registry::PluginRegistry`] is the shared registry both runtimes
//!   register into — tools, commands, flags, shortcuts, providers — with
//!   the upstream conflict semantics (replaceable omission, first-wins
//!   tools/flags, `name:2` command dedup, reserved shortcut keybindings).
//!   Renderer/markdown-transformer registrations ride the JS runtime's own
//!   registry snapshot (`ExtensionHandle::renderer_defs`), which the TUI
//!   consumes directly.
//! - [`host::PluginHost::emit`] is the ONE ordered event dispatch: enabled
//!   builtins → JS adapter → inline factories, merged by
//!   [`merge`] with upstream per-event outcome semantics.
//! - [`loader`] implements the `builtin:<name>` resource scheme,
//!   `disabledBuiltinExtensions`, `--no-extensions` semantics, and path
//!   ordering (CLI → project → global, deduped by canonical path).
//! - [`builtin::BUILTIN_EXTENSION_FACTORIES`] is the Rust mirror of
//!   upstream's `extensionFactories = [...builtInExtensions, ...]`; the
//!   doc comment there records the exact place a new Rust builtin is
//!   added.
//!
//! The QuickJS `ExtensionHandle` actor (thread-pinned runtime, mailbox
//! commands) stays as-is: it is the JS adapter's engine, and the JS
//! handler-order/merge behavior remains implemented inside the runtime.

pub mod builtin;
#[cfg(feature = "extensions")]
pub mod codemode;
// The shared host-service contract: `PluginContext` (the bound service
// facade) and the service traits it exposes. Canonical home; `plugin`
// re-exports both for the historical `plugins::plugin::{context,services}`
// paths.
pub mod context;
pub mod coverage;
pub mod host;
pub mod loader;
pub mod merge;
pub mod plugin;
pub mod registry;
pub mod services;
pub mod source;
#[cfg(feature = "subagent")]
pub mod subagent;
// Type-level gated with the `tool-repair` feature: a build without it compiles
// none of the builtin (plugin + pure repair core; 5 files / ~4.3k lines). The core
// session's assistant `message_end` seam (`agent_session/tool_repair.rs`)
// stays compiled and returns `None` without the feature (the `None`-able
// seam), and the builtin's factory entry in
// [`builtin::BUILTIN_EXTENSION_FACTORIES`] is gated the same way, so
// `builtin:tool-repair` reports the missing-feature diagnostic.
#[cfg(feature = "tool-repair")]
pub mod tool_repair;

pub use builtin::{
    builtin_extension_def, builtin_names, instantiate_builtin, BuiltinExtensionDef,
    BUILTIN_EXTENSION_FACTORIES,
};
#[cfg(feature = "extensions")]
pub use codemode::{CodemodeConfig, CodemodePlugin, CodemodeTool, DEFAULT_INLINE_BUDGET};
pub use coverage::{
    is_native_event, NATIVE_ONLY_EVENTS, UNSUPPORTED_NATIVE_EVENTS, WIRED_NATIVE_EVENTS,
};
pub use host::{JsExtensionsPlugin, PluginHost, RegistrationFailure, ToolCallOutcome};
pub use loader::{
    default_disabled_package_name, disabled_from_flags, emit_default_disabled_package_diagnostics,
    extension_plan_mode, filter_builtin_paths, filter_default_disabled_packages,
    filter_disabled_builtins, only_builtin_paths, order_extension_paths, partition_paths,
    unknown_builtin_errors, DefaultDisabledPackageSkip, ExtensionPlanMode,
    DEFAULT_DISABLED_PACKAGE_NAMES,
};
pub use merge::MergedEmit;
pub use plugin::{
    AgentPlugin, CommandBackend, CommandHandler, EventInterest, InputAction, PluginEvent,
    PluginEventOutcome, PluginFuture, PluginRegistrations, RegistrationContext, ShortcutBackend,
    ShortcutHandler, ToolBackend,
};
pub use registry::{
    DiagnosticKind, PluginDiagnostic, PluginRegistry, RegisteredPlugin, ReservedKeys,
    ResolvedCommand, ResolvedShortcut, RESERVED_KEYBINDING_IDS,
};
pub use source::{builtin_name, is_builtin_path, PluginInfo, BUILTIN_PATH_PREFIX};
#[cfg(feature = "subagent")]
pub use subagent::{
    aggregate_parallel_text, apply_previous, discover_agents, find_nearest_project_agents_dir,
    get_final_output, is_failed, load_agents_from_dir, parse_agent_markdown, result_output,
    truncate_parallel_output, AgentConfig, AgentFrontmatter, AgentScope, AgentSource, Discovery,
    SingleResult, SubagentPlugin, SubagentTool, UsageTotals,
};
/// The `tool-repair` builtin's ported surface (plugin type, pure repair core,
/// config): re-exported only when the feature compiles the module.
#[cfg(feature = "tool-repair")]
pub use tool_repair::{
    aliases::{has_anchor_bleed_bug, has_grammar_leak_bug},
    config::{GrammarRepairConfig, GrammarRepairMode, ToolRepairConfig},
    grammar::{
        parse_tool_grammar_leaks, repair_message_grammar_leaks, resolve_grammar_repair_for_model,
        GrammarRepairResult, RecoveredToolCall,
    },
    repair::{
        normalize_phantom_tool_use, repair_message, repair_message_with_grammar,
        repair_tool_call_arguments, strip_anchor_bleed, strip_grammar_token_leaks,
        wrap_root_string_as_object, LiveRepairStatus, LiveToolRepair, LiveToolSchema,
        MessageRepairOutcome, RepairReport, SchemaCheck, ToolCallRepair, PHANTOM_TOOL_USE_ERROR,
    },
};
/// The `tool-repair` builtin's plugin type and factory — gated with the
/// builtin (and with the module above).
#[cfg(feature = "tool-repair")]
pub use tool_repair::{new_tool_repair_plugin, ToolRepairPlugin};
