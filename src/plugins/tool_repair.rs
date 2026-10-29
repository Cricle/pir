//! The `tool-repair` builtin plugin — `pi-tool-repair` ported from its JS
//! package into a native Rust builtin.
//!
//! Upstream tracking point: npm **`pi-tool-repair` 0.3.6**
//! (monotykamary/pi-tool-repair; installed at
//! `~/.pi/agent/npm/node_modules/pi-tool-repair`). Behavior truth is the
//! package source — `src/index.ts:111-134` (the Phase 0 schema sanitizer),
//! `:136-231` (the two in-place strip passes),
//! `:400-434` (phantom normalization), `:709-963` (live-schema repair,
//! logging), `src/grammar-repair.ts` (the ten-family grammar-leak pass, ported
//! in [`grammar`]) — plus `tool-repair.ts:73-105` (the
//! `before_provider_request` handler) and `:109-173` (the `message_end`
//! pipeline order), the README rule/alias tables, and the family fixtures in
//! the
//! package's `docs/tool-call-grammar-leakage-survey.md`; the package ships no
//! tests (`package.json` `files`), as the scope doc records. The
//! `src/index.ts:476-632` block
//! (`repairToolInput`/`BUILTIN_SCHEMAS`/`validateAgainstSchema`) is dead glue
//! in the upstream package and is deliberately not ported.
//!
//! ## The JS-extension → Rust-builtin conversion path
//!
//! Upstream is a pi JS extension: `message_end` handlers mutate the finalized
//! assistant message in place and, per the package README, the corrected call
//! is what pi persists and resends. The Rust builtin keeps the same pure
//! pipeline ([`repair::repair_message`]) but rides a **dedicated native
//! `message_end` hook** in the agent loop instead of the generic plugin event
//! dispatch (`LoopDeps.message_end_repair`, scope doc §2): the hook is the
//! native face of the SAME pipeline, so the loop only ever runs one
//! `tool-repair` face (`plugins::loader::tool_repair_precedence`) — a JS
//! `message_end` return now lands generically (agent_loop `emit_message_end`),
//! and the JS `pi-tool-repair` package is the fallback face when the builtin
//! is disabled. The hook is deliberately Step 1b.
//!
//! ## Step slicing (per the scope doc)
//!
//! - **Step 1a (landed)**: the pure repair core ([`repair`], [`aliases`],
//!   [`config`]) and its test suite. The plugin registers nothing and is
//!   **not** in
//!   [`BUILTIN_EXTENSION_FACTORIES`](super::builtin::BUILTIN_EXTENSION_FACTORIES)
//!   yet.
//! - **Step 1b (landed)**: the loop hook (`agent_loop.rs`), session wiring
//!   (`agent_session`), `print.rs`, and the `builtin.rs` entry
//!   (`name: "tool-repair"`, replaceable, factory
//!   [`new_tool_repair_plugin`]). The hook calls
//!   [`repair::repair_message_with_grammar`], which owns the full pipeline.
//! - **Step 2 (landed)**: grammar-repair (`grammar-repair.ts`, 1225 lines) in
//!   [`grammar`] — the ten families, the safety gates, the recover/strip
//!   modes, and the message pass wired as pipeline step 3 between the anchor
//!   strip and the live-schema repair
//!   ([`repair::repair_message_with_grammar`]).
//! - **Step 3 (landed)**: the native `before_provider_request` face — the
//!   anchor-bleed schema sanitizer
//!   ([`repair::sanitize_request_payload`], `tool-repair.ts:73-105`) on the
//!   unified plugin event dispatch, gated on the model id the host pushes
//!   through [`AgentPlugin::on_session_context`]. The JS-package suppression
//!   for this face is the native-first precedence in
//!   [`crate::plugins::loader`] ([`crate::plugins::loader::tool_repair_precedence`]):
//!   with `builtin:tool-repair` on the effective plan the JS
//!   `pi-tool-repair` entry is dropped from the plan and the runtime load
//!   list, so the two Phase 0 copies cannot double-run; with the builtin
//!   disabled (`disabledBuiltinExtensions`) the JS entry stays — the
//!   fallback face.
//!   Known gap **closed**: the print host now routes `before_provider_request`
//!   through the host's unified dispatch (`print_transport_hooks` in
//!   `src/coding_agent/print.rs`, mirroring the session's
//!   `agent_session/stream.rs` shape) and pushes a model snapshot before any
//!   request, so the native handler runs in print mode too — with the JS
//!   adapter still dispatched exactly once (the host owns the handle).
//!
//! ## Documented deviations from the JS package
//!
//! - **Real core-tool schemas (net gain)**: the JS path received
//!   `parameters: null` for core tools
//!   (`src/extensions/runtime/host_functions/session_fns.rs:337-380`), so its
//!   live-schema step was a no-op there. The Rust builtin is fed the real
//!   `AgentTool::parameters()` values, so the repair actually engages on
//!   `read`/`edit`/`grep`/`find`/`ls`/`bash`/`write` (scope doc §0.5).
//! - **Bare-string roots**: pir's `ToolCall.arguments` is a
//!   `BTreeMap<String, JsonValue>` and the provider adapters parse a non-object
//!   arguments payload into an empty map, so the TS bare-string root can no
//!   longer be represented once a message reaches `message_end`.
//!   `wrapRootStringAsObject` is still ported (exercised through the per-call
//!   API) but cannot fire from the message pipeline.
//! - **Validator seam**: TS calls TypeBox `Value.Check` inline; the Rust core
//!   takes the live-schema check as an injected callback because the preflight
//!   validator (`agent/tool_scheduler/prepare.rs`) is module-private. See
//!   [`repair`]'s module docs — Step 1b exposes it; no parallel validator is
//!   implemented here.
//! - **Debug logging**: the package prints a multi-line block per repair
//!   (`src/index.ts:949-963`); the builtin keeps the same information level on
//!   a single stderr line per repair.
//! - **Config loading**: `ToolRepairConfig` is constructed lazily
//!   ([`ToolRepairPlugin::config`]) — never at plugin/struct construction —
//!   because Step 1b wires the plugin and no code path may touch the
//!   filesystem before that.

pub mod aliases;
pub mod config;
pub mod grammar;
pub mod repair;

pub use grammar::{
    parse_tool_grammar_leaks, repair_message_grammar_leaks, resolve_grammar_repair_for_model,
    GrammarRepairResult, RecoveredToolCall,
};

use std::sync::{Arc, OnceLock, RwLock};

use serde_json::Value as JsonValue;

use crate::extensions::SessionContextSnapshot;
use crate::plugins::plugin::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture,
};
use crate::plugins::source::PluginInfo;

/// The `tool-repair` builtin plugin: identity plus the lazily loaded config.
///
/// It registers no tool/command/flag (the JS package registers none either);
/// its work is the assistant `message_end` repair pipeline, delivered through
/// the loop's native repair hook (`message_end_repair`), plus the Phase 0
/// `before_provider_request` schema sanitizer on the unified plugin event
/// dispatch.
pub struct ToolRepairPlugin {
    info: PluginInfo,
    /// Lazily loaded package config (env + `extensions/pi-tool-repair.json`);
    /// `None` until the first access so construction is side-effect free.
    config: OnceLock<config::ToolRepairConfig>,
    /// The model id the host last pushed (`on_session_context`), the model
    /// gate for the `before_provider_request` handler. `None` before the first
    /// push (and for hosts that push a non-model snapshot): the gate then
    /// never matches, which is the safe direction — an unpushed session
    /// behaves exactly like the JS package's `ctx.model` being undefined.
    model_id: RwLock<Option<String>>,
}

impl ToolRepairPlugin {
    fn new() -> Self {
        let mut info = PluginInfo::builtin("tool-repair");
        // Mirrors the builtin table's `replaceable` flag. Inert for name-based
        // omission (this plugin claims no tool/command/flag names); the
        // native-vs-JS `tool-repair` precedence is decided on the effective
        // extension plan in `plugins::loader`
        // (`loader::tool_repair_precedence`), not here.
        info.replaceable =
            super::builtin_extension_def("tool-repair").is_some_and(|def| def.replaceable);
        Self {
            info,
            config: OnceLock::new(),
            model_id: RwLock::new(None),
        }
    }

    /// The lazily loaded config: the `PI_TOOL_REPAIR_DEBUG` env flag plus
    /// `agent_dir()/extensions/pi-tool-repair.json` (package defaults when the
    /// file is absent). Step 1b's hook reads
    /// [`ToolRepairConfig::debug`](config::ToolRepairConfig::debug) from here
    /// and passes it to [`repair::repair_message`].
    pub fn config(&self) -> &config::ToolRepairConfig {
        self.config.get_or_init(config::ToolRepairConfig::load)
    }
}

impl AgentPlugin for ToolRepairPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// The one event this plugin consumes: `before_provider_request` (Phase 0).
    /// Declaring the interest is what makes the session host install its
    /// per-request transport hook (`agent_session/stream.rs`'s
    /// `has_handlers` gate); a session without it never parses/unwraps the
    /// request payload for this plugin.
    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["before_provider_request".to_owned()])
    }

    /// Cache the model id for the Phase 0 gate.
    ///
    /// The snapshot's `model` is either the serialized `Model` object (the
    /// per-prompt push) or the configured model-id string
    /// (`agent_session/runtime_loader.rs:80-84`); both shapes resolve to an id.
    /// A malformed/absent value clears the gate (safe: no repair without a
    /// known model, mirroring the TS `if (!model) return`).
    fn on_session_context(&self, snapshot: &SessionContextSnapshot) {
        let id = match &snapshot.model {
            JsonValue::String(id) => Some(id.clone()),
            JsonValue::Object(map) => map.get("id").and_then(JsonValue::as_str).map(str::to_owned),
            _ => None,
        };
        *self
            .model_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = id.filter(|id| !id.is_empty());
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            if event.name.as_ref() != "before_provider_request" {
                return PluginEventOutcome::None;
            }
            let model = self
                .model_id
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let Some(payload) = event.payload.get_mut("payload") else {
                return PluginEventOutcome::None;
            };
            // Gate + sanitize in one pure pass: a non-gated model (or a payload
            // with no tools/response_format) returns `false` and the handler
            // has no opinion, so the host keeps its original request body.
            if !repair::sanitize_request_payload(payload, model.as_deref()) {
                return PluginEventOutcome::None;
            }
            // Mirror the TS handler's `return payload`: the session host reads
            // the MERGED RESULT (`agent_session/stream.rs:124-131`), not the
            // in-place payload, so the sanitized body must come back as the
            // outcome too. The in-place rewrite already happened, so later
            // plugins (and `EmitOutcome.payload` observers) see the sanitized
            // payload as well.
            PluginEventOutcome::Generic(
                event
                    .payload
                    .get("payload")
                    .cloned()
                    .unwrap_or(JsonValue::Null),
            )
        })
    }

    /// Downcast support: the host reaches this concrete builtin to attach the
    /// native `message_end` repair hook (the `mcp`/`subagent` builtin seam).
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

/// Factory for
/// [`super::builtin::BUILTIN_EXTENSION_FACTORIES`](super::builtin::BUILTIN_EXTENSION_FACTORIES)
/// (Step 1b appends the entry; the builtin name is `tool-repair`, kebab-case
/// like `tool-search`).
pub fn new_tool_repair_plugin() -> Arc<dyn AgentPlugin> {
    Arc::new(ToolRepairPlugin::new())
}
