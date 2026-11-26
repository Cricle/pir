//! Independent, host-neutral core of the `memory` builtin.
//!
//! The `cm-memory` store, its SQLite backend and the settings-driven store
//! lifecycle stay host-side. This crate owns the pure prompt envelope: the
//! recall block appended to a user prompt and the stable brief inserted into
//! the system prompt sections, both XML-escaped so a memory that contains
//! instructions (or a forged closing tag) cannot break out of the data
//! envelope.
//!
//! It depends on `pir-plugin-api` and third-party crates only — never on
//! `pir-core`.

#![deny(unsafe_code)]

pub mod prompt;

pub use prompt::{append_to_prompt, render_memory_brief, render_memory_context, xml_escape};

use pir_plugin_api::{BuiltinPlugin, PluginInfo};

/// The registered memory tool name.
pub const TOOL_NAME: &str = "memory";

/// The `/memory` command name.
pub const COMMAND_NAME: &str = "memory";

/// Minimal protocol-facing shell for the memory builtin.
pub struct MemoryPlugin {
    info: PluginInfo,
}

impl MemoryPlugin {
    pub fn new() -> Self {
        Self {
            info: PluginInfo::builtin(PLUGIN_NAME),
        }
    }
}

impl BuiltinPlugin for MemoryPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
}

pir_plugin_api::builtin_plugin_shell! {
    plugin = MemoryPlugin,
    name = "memory",
    factory = new_memory_plugin,
}
