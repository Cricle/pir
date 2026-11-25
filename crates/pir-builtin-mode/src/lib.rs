//! Independent, host-neutral mode domain for the `mode` builtin.
//!
//! This crate owns the pure foreman/plan/normal mode state: the mode and phase
//! labels, the approval records and journal replay, the delegation routing and
//! approval-identity helpers, and the mode settings shape. It depends on
//! `pir-plugin-api` and third-party crates only — never on `pir-core`.
//!
//! A host integrates the domain by calling these helpers from its plugin,
//! agent and tool-gate implementations. Session state, tool registration and
//! subagent execution stay host-side.

#![deny(unsafe_code)]

pub mod approvals;
pub mod delegation;
pub mod journal;
pub mod mode;
pub mod settings;

pub use approvals::{ApprovalKind, ApprovalRecord, ApprovalState};
pub use delegation::{
    apply_foreman_routing, approval_block_reason, approval_id, approval_kind_label,
    approval_summary, force_readonly_subagent_tools, force_reviewer_subagent_tools, has_reviewer,
    has_worker, plan_approval_block_reason, plan_approval_id, plan_summary, scan_subagent_calls,
    subagent_agents, tool_allowed_in_mode, tool_visible_in_mode, truncate_chars,
    MODE_ALLOWED_TOOLS, PLAN_CHILD_TOOLS, REVIEWER_CHILD_TOOLS, SUBMIT_PLAN_TOOL,
};
pub use journal::{
    call_id_hash, decide_approval_for_session, journal_path, read_state, write_atomic_json,
    ExecutionContract, ForemanJournal, JournalEvent, JournalState, Verification,
};
pub use mode::{Mode, Phase};
pub use settings::{ApprovalMode, ModeSettings, RouterMode};

use pir_plugin_api::{BuiltinPlugin, PluginInfo};

/// Minimal protocol-facing shell for the mode builtin.
pub struct ModePlugin {
    info: PluginInfo,
}

impl ModePlugin {
    pub fn new() -> Self {
        Self {
            info: PluginInfo::builtin(PLUGIN_NAME),
        }
    }
}

impl BuiltinPlugin for ModePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
}

pir_plugin_api::builtin_plugin_shell! {
    plugin = ModePlugin,
    name = "mode",
    factory = new_mode_plugin,
}
