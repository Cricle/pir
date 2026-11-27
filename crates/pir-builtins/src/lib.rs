//! Aggregate catalog for the independent builtin implementation crates.
//!
//! This crate owns the builtin ORDER, the default-enable flags and the
//! host-feature mapping. It depends only on `pir-plugin-api` and the builtin
//! implementation crates — never on `pir-core` or `pir-sdk`, so the engine can
//! consume the catalog without any dependency back into the implementations.
//!
//! The order matches the historical in-core table: `subagent, mode, mcp,
//! codemode, context-mode, tool-search, tool-repair, todo, memory, phone`.
//!
//! ## The default plan is deliberately small
//!
//! Only `todo`, `subagent` and `mode` are default-on. Everything else —
//! `mcp`, `codemode`, `context-mode`, `tool-search`, `tool-repair`, `memory`,
//! `phone` — is default-off and joins a session only when something names it
//! (`-e builtin:<name>`, `settings.extensions`, `+builtin:<name>`, or the
//! `--tools` selection for a tool it registers). The reason is a measured one:
//! the full plan declared 25 tools at 39,530 bytes of `tools[]` schema against
//! upstream's 4 tools / 2,527 bytes, and 48% of that payload was the twelve
//! `ctx_*` tools, which produced **zero** calls across six measured runs of a
//! read-and-edit task. Paying for that much schema on every request to expose
//! tools the model declines is the cost this default avoids. `phone` was
//! already default-off for its own reason (it binds a local port and holds a
//! server task).
//!
//! ## Two projections of one table
//!
//! [`KNOWN_BUILTINS`] is the feature-independent table: order, default-enable
//! flag and the HOST engine's cargo feature for every builtin this workspace
//! knows about. A host reads it to explain a *missing* builtin ("not compiled
//! into this build: the `mcp` feature is off"), which by definition needs rows
//! this build does not have.
//!
//! [`descriptors`] is the same table projected onto this crate's enabled
//! features: the subset that is actually instantiable here. The order and the
//! flags come from [`KNOWN_BUILTINS`], so the two can never drift; a test pins
//! that (`tests/catalog.rs`).

#![deny(unsafe_code)]

use pir_plugin_api::{BuiltinDescriptor, BuiltinPlugin};
use std::sync::Arc;

/// One row of the feature-independent builtin table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownBuiltin {
    pub name: &'static str,
    /// The HOST engine's cargo feature that compiles this builtin into an
    /// engine build. This is `pir-core`'s feature name, not this crate's:
    /// `mode` rides `pir-core/subagent` and `codemode` rides
    /// `pir-core/extensions`.
    pub host_feature: &'static str,
    /// Whether a default extension plan includes this builtin when nothing
    /// names it explicitly.
    pub default_enabled: bool,
    /// This crate's own feature gate (identical to `name` for every builtin
    /// today, but kept explicit so a rename cannot silently change the gate).
    pub feature: &'static str,
}

/// Every builtin this workspace knows about, in registration order, regardless
/// of which features are enabled.
pub const KNOWN_BUILTINS: [KnownBuiltin; 10] = [
    KnownBuiltin {
        name: "subagent",
        host_feature: "subagent",
        default_enabled: true,
        feature: "subagent",
    },
    KnownBuiltin {
        name: "mode",
        host_feature: "subagent",
        default_enabled: true,
        feature: "mode",
    },
    KnownBuiltin {
        name: "mcp",
        host_feature: "mcp",
        default_enabled: false,
        feature: "mcp",
    },
    KnownBuiltin {
        name: "codemode",
        host_feature: "extensions",
        default_enabled: false,
        feature: "codemode",
    },
    KnownBuiltin {
        name: "context-mode",
        host_feature: "context-mode",
        default_enabled: false,
        feature: "context-mode",
    },
    KnownBuiltin {
        name: "tool-search",
        host_feature: "tool-search",
        default_enabled: false,
        feature: "tool-search",
    },
    KnownBuiltin {
        name: "tool-repair",
        host_feature: "tool-repair",
        default_enabled: false,
        feature: "tool-repair",
    },
    KnownBuiltin {
        name: "todo",
        host_feature: "todo",
        default_enabled: true,
        feature: "todo",
    },
    KnownBuiltin {
        name: "memory",
        host_feature: "memory",
        default_enabled: false,
        feature: "memory",
    },
    KnownBuiltin {
        name: "phone",
        host_feature: "phone",
        default_enabled: false,
        feature: "phone",
    },
];

/// The known-builtin row for `name`, whether or not this build has it.
pub fn known(name: &str) -> Option<KnownBuiltin> {
    KNOWN_BUILTINS.iter().copied().find(|row| row.name == name)
}

/// The factory for one builtin, or `None` when this build disabled its
/// feature. The match is exhaustive on purpose: a row added to
/// [`KNOWN_BUILTINS`] without a factory (or vice versa) fails to compile.
fn factory_for(name: &str) -> Option<pir_plugin_api::BuiltinPluginFactory> {
    match name {
        #[cfg(feature = "subagent")]
        "subagent" => Some(subagent),
        #[cfg(feature = "mode")]
        "mode" => Some(mode),
        #[cfg(feature = "mcp")]
        "mcp" => Some(mcp),
        #[cfg(feature = "codemode")]
        "codemode" => Some(codemode),
        #[cfg(feature = "context-mode")]
        "context-mode" => Some(context_mode),
        #[cfg(feature = "tool-search")]
        "tool-search" => Some(tool_search),
        #[cfg(feature = "tool-repair")]
        "tool-repair" => Some(tool_repair),
        #[cfg(feature = "todo")]
        "todo" => Some(todo),
        #[cfg(feature = "memory")]
        "memory" => Some(memory),
        #[cfg(feature = "phone")]
        "phone" => Some(phone),
        _ => None,
    }
}

/// Every builtin this crate can instantiate, in registration order.
///
/// The descriptor's order, `replaceable` flag, default-enable flag and host
/// feature all come from [`KNOWN_BUILTINS`]; only the factory is decided by the
/// feature gates. A row with no factory here is skipped; `pir-core` keeps the
/// authoritative factory table for the engine's own builtins.
pub fn descriptors() -> Vec<BuiltinDescriptor> {
    KNOWN_BUILTINS
        .iter()
        .filter_map(|row| {
            let factory = factory_for(row.name)?;
            Some(BuiltinDescriptor {
                name: row.name,
                feature: row.host_feature,
                replaceable: true,
                default_enabled: row.default_enabled,
                factory,
            })
        })
        .collect()
}

/// Registered builtin names in order.
pub fn names() -> Vec<&'static str> {
    descriptors().iter().map(|entry| entry.name).collect()
}

/// Names the default plan enables: `todo`, `subagent`, `mode`. Every other
/// builtin joins a session only when something names it explicitly.
pub fn default_names() -> Vec<&'static str> {
    descriptors()
        .into_iter()
        .filter(|entry| entry.default_enabled)
        .map(|entry| entry.name)
        .collect()
}

/// Look up one descriptor by name, among the builtins this build has.
pub fn descriptor(name: &str) -> Option<BuiltinDescriptor> {
    descriptors().into_iter().find(|entry| entry.name == name)
}

/// Instantiate one builtin by name.
pub fn instantiate(name: &str) -> Option<Arc<dyn BuiltinPlugin>> {
    descriptor(name).map(|entry| (entry.factory)())
}

// ---------------------------------------------------------------------------
// Factories
// ---------------------------------------------------------------------------

#[cfg(feature = "subagent")]
fn subagent() -> Arc<dyn BuiltinPlugin> {
    pir_builtin_subagent::new_subagent_plugin()
}
#[cfg(feature = "mode")]
fn mode() -> Arc<dyn BuiltinPlugin> {
    pir_builtin_mode::new_mode_plugin()
}
#[cfg(feature = "mcp")]
fn mcp() -> Arc<dyn BuiltinPlugin> {
    pir_builtin_mcp::new_mcp_plugin()
}
#[cfg(feature = "codemode")]
fn codemode() -> Arc<dyn BuiltinPlugin> {
    pir_builtin_codemode::new_codemode_plugin()
}
#[cfg(feature = "context-mode")]
fn context_mode() -> Arc<dyn BuiltinPlugin> {
    pir_builtin_context_mode::new_context_mode_plugin()
}
#[cfg(feature = "tool-search")]
fn tool_search() -> Arc<dyn BuiltinPlugin> {
    pir_builtin_tool_search::new_tool_search_plugin()
}
#[cfg(feature = "tool-repair")]
fn tool_repair() -> Arc<dyn BuiltinPlugin> {
    pir_builtin_tool_repair::new_tool_repair_plugin()
}
#[cfg(feature = "todo")]
fn todo() -> Arc<dyn BuiltinPlugin> {
    pir_builtin_todo::new_todo_plugin()
}
#[cfg(feature = "memory")]
fn memory() -> Arc<dyn BuiltinPlugin> {
    pir_builtin_memory::new_memory_plugin()
}
#[cfg(feature = "phone")]
fn phone() -> Arc<dyn BuiltinPlugin> {
    pir_builtin_phone::new_phone_plugin()
}
