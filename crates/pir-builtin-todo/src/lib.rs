//! Independent, host-neutral todo domain.
//!
//! This crate is the pure core of the `todo` builtin: canonical task/state
//! types, the status machine, dependency-cycle detection, argument parsing,
//! the pure reducer, response formatting, transcript replay, and an
//! LRU-bounded per-session [`TodoStore`]. It depends only on
//! `pir-plugin-api` and third-party crates — no `pir-core` types.
//!
//! The protocol-facing shell at the bottom of this module is the minimal
//! [`BuiltinPlugin`] identity; a host integrates the domain by calling
//! [`TodoStore::dispatch`] from its tool implementation and
//! [`TodoStore::replay`] when a transcript push arrives.

#![deny(unsafe_code)]

pub mod constants;
pub mod envelope;
pub mod format;
pub mod graph;
pub mod mutation;
pub mod params;
pub mod replay;
pub mod sanitize;
pub mod store;
pub mod transition;
pub mod types;

pub use constants::{
    COMMAND_NAME, DEFAULT_PROMPT_GUIDELINES, DEFAULT_PROMPT_SNIPPET, ERR_REQUIRES_INTERACTIVE,
    MAX_ACTIVE_FORM_CHARS, MAX_BATCH_ITEMS, MAX_DESCRIPTION_CHARS, MAX_METADATA_BYTES,
    MAX_OWNER_CHARS, MAX_SESSION_SLOTS, MAX_SUBJECT_CHARS, MAX_TASKS_PER_SESSION, MSG_NO_TODOS,
    TOOL_DESCRIPTION, TOOL_LABEL, TOOL_NAME,
};
pub use envelope::{build_response, build_response_for, TodoResponse};
pub use format::{
    format_command_task_line, format_content, format_get_lines, format_list_line,
    format_todos_command,
};
pub use graph::{derive_blocks, detect_cycle};
pub use mutation::apply_mutation;
pub use params::{parse_batch_items, BatchItem, TodoParams};
pub use replay::{entries_fingerprint, replay_from_entries, snapshot_from_details, ReplayScan};
pub use sanitize::sanitize_terminal_text;
pub use store::{TodoStats, TodoStatsSnapshot, TodoStore};
pub use transition::is_transition_valid;
pub use types::{Task, TaskAction, TaskStatus, TodoOp, TodoState};

use pir_plugin_api::{BuiltinPlugin, PluginInfo};

/// Minimal protocol-facing shell for the todo builtin.
pub struct TodoPlugin {
    info: PluginInfo,
}

impl TodoPlugin {
    pub fn new() -> Self {
        Self {
            info: PluginInfo::builtin(TOOL_NAME),
        }
    }
}

impl BuiltinPlugin for TodoPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
}

pir_plugin_api::builtin_plugin_shell! {
    plugin = TodoPlugin,
    name = "todo",
    factory = new_todo_plugin,
}
