//! Independent protocol-facing shell for the deferred-tool search builtin.

#![deny(unsafe_code)]

mod search;

use pir_plugin_api::{BuiltinPlugin, PluginInfo};

pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

pub struct ToolSearchPlugin {
    info: PluginInfo,
}

impl ToolSearchPlugin {
    pub fn new() -> Self {
        Self {
            info: PluginInfo::builtin("tool-search"),
        }
    }
}

impl BuiltinPlugin for ToolSearchPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
}

pub use search::{
    tokenize, tool_document, tool_search_document_text, Bm25Document, Bm25Ranker, ToolDocumentText,
    ToolNamespace,
};

pir_plugin_api::builtin_plugin_shell! {
    plugin = ToolSearchPlugin,
    name = "tool-search",
    factory = new_tool_search_plugin,
}
