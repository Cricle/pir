//! Independent, host-neutral core of the `context-mode` builtin.
//!
//! Pure pieces extracted from `pir-core`'s context-mode plugin: `ctx_batch`
//! request validation and source labelling, HTTP/text fetch normalization
//! (content-type mapping, body decoding, HTML to text), document chunking, and
//! journal input shaping.
//!
//! This crate intentionally depends on no project crate other than the
//! engine-free `pir-plugin-api` protocol (plus `serde_json`). Host-specific
//! work stays in `pir-core`: shell spawning, `reqwest` fetching, SQLite/FTS5
//! storage and tool registration. The host adapts these helpers from its tool
//! implementations; nothing here touches I/O or the network.

#![deny(unsafe_code)]

pub mod batch;
pub mod chunk;
pub mod fetch;
pub mod journal;

pub use batch::{
    batch_source, cap_reply, BatchRequest, DEFAULT_TIMEOUT_SECS, HITS_PER_QUERY, MAX_COMMANDS,
    MAX_QUERY_BYTES, MAX_SEARCH_QUERIES, MAX_TIMEOUT_SECS, OUTPUT_CAP,
};
pub use chunk::{chunk_document, IndexDoc, ParsedChunk};
pub use fetch::{
    content_type_for, decode_body, html_to_text, is_http_scheme, normalize_response_body,
    validate_http_scheme, FETCH_TIMEOUT_SECS, MAX_BODY_BYTES, MAX_REDIRECTS,
};
pub use journal::{
    JournalInput, SESSION_JOURNAL_MAX_BYTES, SESSION_JOURNAL_MAX_ROWS, SESSION_JOURNAL_TTL_SECS,
};

use pir_plugin_api::{BuiltinPlugin, PluginInfo};
use std::sync::Arc;

/// Tool names owned by the context-mode builtin (the host registers the tool
/// implementations; the names stay stable here).
pub const CTX_BATCH_TOOL_NAME: &str = "ctx_batch";
pub const CTX_FETCH_AND_INDEX_TOOL_NAME: &str = "ctx_fetch_and_index";

/// Truncate `text` on a UTF-8 boundary (the index and tool results store valid
/// text; a byte-index cut inside a multi-byte char would panic).
pub fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Truncate to at most `max` characters, trimming trailing whitespace.
pub fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((byte_index, _)) => text[..byte_index].trim_end().to_owned(),
        None => text.to_owned(),
    }
}

/// Minimal protocol-facing shell for the context-mode builtin.
pub struct ContextModePlugin {
    info: PluginInfo,
}

impl ContextModePlugin {
    pub fn new() -> Self {
        Self {
            info: PluginInfo::builtin("context-mode"),
        }
    }
}

impl Default for ContextModePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl BuiltinPlugin for ContextModePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
}

pub fn new_context_mode_plugin() -> Arc<dyn BuiltinPlugin> {
    Arc::new(ContextModePlugin::new())
}
