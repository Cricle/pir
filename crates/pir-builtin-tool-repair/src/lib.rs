//! Independent, engine-free package for the `tool-repair` builtin.
//!
//! This crate owns the pure repair pipeline ported from `pi-tool-repair`
//! 0.3.6 and the host-facing plugin shell. It depends on `pir-plugin-api` and
//! third-party crates only — never on `pir-core` or `pir-sdk` — so the engine
//! can consume it through neutral DTOs:
//!
//! - [`types`] mirrors the handful of message shapes the passes interpret
//!   (`AssistantMessage`, `ContentBlock`, `ToolCall`, `StopReason`, …) and
//!   preserves every other message key for lossless round-tripping. The host
//!   adapts a core `ai::types::AssistantMessage` with
//!   [`AssistantMessage::from_json_value`] / [`AssistantMessage::to_json_value`]
//!   and maps it back.
//! - [`aliases`] holds the static tables (field aliases, string-arg tools,
//!   model regex sets, grammar names).
//! - [`config`] parses and normalizes the package config. The agent-dir
//!   resolution is the host's job: callers pass the directory to
//!   [`config::ToolRepairConfig::config_path_in`] /
//!   [`config::ToolRepairConfig::load_from_agent_dir`].
//! - [`grammar`] is the ten-family grammar-leak pass (strip/recover, gates,
//!   per-family parsers).
//! - [`repair`] is the full `message_end` pipeline: the GLM token strip, the
//!   anchor-bleed strip, grammar recovery, live-schema repair, and the
//!   phantom-toolUse normalization, plus the Phase 0
//!   `before_provider_request` schema-anchor sanitizer.
//!
//! The live-schema validator stays an injected callback
//! ([`repair::SchemaCheck`]); this package ships no JSON-Schema validator and
//! leaves the host's real preflight validator as the single source of truth.
//!
//! ## Host-facing shell
//!
//! [`ToolRepairPlugin`] implements the neutral
//! [`pir_plugin_api::BuiltinPlugin`] contract: identity plus the cached session
//! model id, with [`ToolRepairPlugin::sanitize_before_provider_request`] as the
//! core-free Phase 0 entry point. The engine's `AgentPlugin` wrapper — the
//! `before_provider_request` event-payload indirection and the outcome echo,
//! the native `message_end` loop hook, and the JavaScript-package precedence —
//! is deliberately not ported here; it is the host adapter's job and lives
//! outside this package.
//!
//! ## Documented deviations from the engine copy
//!
//! - **Debug diagnostics go to stderr.** Every call site keeps the engine
//!   copy's `debug` gate, but the lines are written through [`diag`] instead of
//!   the engine's routed, repeat-collapsed `ext_diag_info!` / `ext_diag!`
//!   macros. Repair results never depend on logging; host-routed sinks are a
//!   later adapter concern.
//! - **Config loading needs the agent dir passed in.** The engine copy resolved
//!   it with `ai::providers::agent_dir()`; that policy stays in the host.
//! - **The plugin shell is protocol-level only.** `BuiltinPlugin::on_event`
//!   keeps its default no-op: the neutral API defines no payload envelope for
//!   `before_provider_request`, so the host adapter decides where the request
//!   body sits and calls [`repair::sanitize_request_payload`] or
//!   [`ToolRepairPlugin::sanitize_before_provider_request`].
//! - **The plugin does not load config lazily.** There is no host `on_init`
//!   equivalent in `pir_plugin_api`; callers pass a
//!   [`config::ToolRepairConfig`] to the pipeline explicitly.

#![deny(unsafe_code)]

pub mod aliases;
pub mod config;
pub mod diag;
pub mod grammar;
pub mod repair;
pub mod types;

pub use grammar::{
    parse_tool_grammar_leaks, repair_message_grammar_leaks, resolve_grammar_repair_for_model,
    GrammarRepairResult, RecoveredToolCall,
};
pub use repair::{
    normalize_phantom_tool_use, repair_message, repair_message_with_grammar,
    repair_tool_call_arguments, sanitize_request_payload, LiveRepairStatus, LiveToolRepair,
    LiveToolSchema, MessageRepairOutcome, RepairReport, SchemaCheck, ToolCallRepair,
    PHANTOM_TOOL_USE_ERROR,
};
pub use types::{
    AssistantMessage, AssistantRole, ContentBlock, ImageContent, StopReason, TextContent,
    ThinkingContent, ToolCall,
};

use std::path::Path;
use std::sync::RwLock;

use pir_plugin_api::{BuiltinPlugin, PluginInfo, SessionContextSnapshot};
use serde_json::Value as JsonValue;

/// The builtin's name (`builtin:tool-repair`).
pub const TOOL_REPAIR_PLUGIN_NAME: &str = "tool-repair";

/// The one event the host-facing Phase 0 face consumes.
pub const BEFORE_PROVIDER_REQUEST_EVENT: &str = "before_provider_request";

/// The `tool-repair` builtin shell: identity plus the session model gate.
///
/// It registers no tool/command/flag; its pure work is delivered by the
/// [`repair`] and [`grammar`] functions, called by the host adapter at the
/// `message_end` and `before_provider_request` seams.
pub struct ToolRepairPlugin {
    info: PluginInfo,
    /// The model id the host last pushed through
    /// [`BuiltinPlugin::on_session_context`]. `None` before the first push, so
    /// the Phase 0 gate never matches — the safe direction, matching the JS
    /// package's `ctx.model === undefined`.
    model_id: RwLock<Option<String>>,
}

impl ToolRepairPlugin {
    pub fn new() -> Self {
        let mut info = PluginInfo::builtin(TOOL_REPAIR_PLUGIN_NAME);
        // Mirrors the `pir-builtins` descriptor (`replaceable: true`).
        info.replaceable = true;
        Self {
            info,
            model_id: RwLock::new(None),
        }
    }

    /// The model id the host last pushed, if any.
    pub fn model_id(&self) -> Option<String> {
        self.model_id
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Resolve the model id out of a neutral session snapshot: either the
    /// configured model-id string or the serialized model object's `id`.
    pub fn model_id_from_snapshot(snapshot: &SessionContextSnapshot) -> Option<String> {
        model_id_from_snapshot_value(&snapshot.model)
    }

    /// Whether the Phase 0 handler could rewrite a request body for `model`.
    ///
    /// Both the caller's model and the cached session model are checked
    /// (a summarizer/compaction request can use a different model), and a
    /// match on either side answers `true`. This is the exact inertness test a
    /// host hook-install fast path needs.
    pub fn is_active_for_model(&self, model: Option<&str>) -> bool {
        phase0_active_for(model, self.model_id().as_deref())
    }

    /// The Phase 0 pure pass for the cached session model. Returns whether
    /// `payload` changed. The host adapter decides where the provider request
    /// body lives and whether a changed body must be returned as the event
    /// outcome.
    pub fn sanitize_before_provider_request(&self, payload: &mut JsonValue) -> bool {
        let model = self.model_id();
        repair::sanitize_request_payload(payload, model.as_deref())
    }

    /// Load this package's config from an explicitly resolved agent dir: the
    /// env-backed debug flag plus `<agent_dir>/extensions/pi-tool-repair.json`
    /// (package defaults when the file is absent).
    pub fn config_from_agent_dir(agent_dir: &Path) -> config::ToolRepairConfig {
        config::ToolRepairConfig::load_from_agent_dir(agent_dir)
    }
}

impl BuiltinPlugin for ToolRepairPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// Cache the model id for the Phase 0 gate. A malformed/absent value
    /// clears the gate (safe: no repair without a known model).
    fn on_session_context(&self, snapshot: &SessionContextSnapshot) {
        *self
            .model_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Self::model_id_from_snapshot(snapshot);
    }
}

pir_plugin_api::builtin_plugin_shell! {
    plugin = ToolRepairPlugin,
    name = "tool-repair",
    factory = new_tool_repair_plugin,
}

/// Whether the Phase 0 schema sanitizer could rewrite a request body.
///
/// The host's hook-install fast path and the plugin's own predicate must agree
/// exactly, so the rule is one free function: a match on EITHER the caller's
/// model or the cached session model answers `true` (a summarizer/compaction
/// request can legitimately run under a different model than the session one).
/// Both sides are checked conservatively — a needless value-body path is safe,
/// skipping a live handler is not.
pub fn phase0_active_for(caller_model: Option<&str>, cached_model: Option<&str>) -> bool {
    aliases::has_anchor_bleed_bug(caller_model) || aliases::has_anchor_bleed_bug(cached_model)
}

/// Whether the Phase 0 handler is inert for `model`: the negated
/// [`phase0_active_for`] with no cached session model.
pub fn phase0_inert_for(model: Option<&str>) -> bool {
    !phase0_active_for(model, None)
}

/// Resolve a model id out of a session snapshot's `model` field.
///
/// Two shapes reach a host: the serialized `Model` object (the per-prompt push)
/// and the configured model-id string. Both resolve to an id; an empty or
/// absent value is `None` (the safe direction — no repair without a known
/// model).
pub fn model_id_from_snapshot_value(model: &JsonValue) -> Option<String> {
    let id = match model {
        JsonValue::String(id) => Some(id.clone()),
        JsonValue::Object(map) => map.get("id").and_then(JsonValue::as_str).map(str::to_owned),
        _ => None,
    };
    id.filter(|id| !id.is_empty())
}
