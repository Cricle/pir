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
//!   `crates/pir-core/src/coding_agent/print.rs`, mirroring the session's
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

// The pure repair pipeline (aliases, config, grammar, repair) lives in the
// engine-free `pir-builtin-tool-repair` crate. The modules below re-export it
// under the historical `pir::plugins::tool_repair::{aliases,config,grammar,
// repair,types}` paths and shadow the four message-taking entry points with
// engine-typed shims, so there is exactly one implementation and no caller has
// to know the crate exists.
pub use pir_builtin_tool_repair::{aliases, config, types};

/// The crate's static tables and pure message-independent helpers.
pub mod grammar {
    pub use pir_builtin_tool_repair::grammar::*;

    use std::collections::BTreeSet;

    use crate::ai::types::AssistantMessage;

    use super::api;
    use super::config::GrammarRepairConfig;

    /// [`super::grammar::repair_message_grammar_leaks`] over an ENGINE message,
    /// in place.
    ///
    /// Shadows the glob re-export above on purpose: the crate's copy takes its
    /// neutral DTO, and this engine has one message type its callers hold.
    pub fn repair_message_grammar_leaks(
        message: &mut AssistantMessage,
        config: &GrammarRepairConfig,
        known_tools: &BTreeSet<String>,
    ) -> super::grammar::GrammarRepairResult {
        api::repair_message_grammar_leaks(message, config, known_tools)
    }
}

/// The repair pipeline entry points.
pub mod repair {
    pub use pir_builtin_tool_repair::repair::*;

    use std::collections::BTreeSet;

    use serde_json::Value as JsonValue;

    use crate::ai::types::AssistantMessage;

    use super::api;
    use super::config::GrammarRepairConfig;

    /// [`super::repair::repair_message`] over an ENGINE message, in place.
    pub fn repair_message(
        message: &mut AssistantMessage,
        model: Option<&str>,
        tools: &[super::repair::LiveToolSchema],
        check: &dyn Fn(&JsonValue, &JsonValue) -> super::repair::SchemaCheck,
        debug: bool,
    ) -> super::repair::MessageRepairOutcome {
        api::repair_message(message, model, tools, check, debug)
    }

    /// [`super::repair::repair_message_with_grammar`] over an ENGINE message,
    /// in place.
    #[allow(clippy::too_many_arguments)]
    pub fn repair_message_with_grammar(
        message: &mut AssistantMessage,
        model: Option<&str>,
        tools: &[super::repair::LiveToolSchema],
        grammar_config: &GrammarRepairConfig,
        known_tools: &BTreeSet<String>,
        check: &dyn Fn(&JsonValue, &JsonValue) -> super::repair::SchemaCheck,
        debug: bool,
    ) -> super::repair::MessageRepairOutcome {
        api::repair_message_with_grammar(
            message,
            model,
            tools,
            grammar_config,
            known_tools,
            check,
            debug,
        )
    }

    /// [`super::repair::normalize_phantom_tool_use`] over an ENGINE message.
    pub fn normalize_phantom_tool_use(message: &mut AssistantMessage) -> bool {
        api::normalize_phantom_tool_use(message)
    }
}

pub use grammar::{
    parse_tool_grammar_leaks, resolve_grammar_repair_for_model, GrammarRepairResult,
    RecoveredToolCall,
};

/// The engine-facing repair surface: the four pipeline entry points that take
/// an assistant message, with the engine's own [`AssistantMessage`] type.
///
/// The crate is engine-free, so its copies take its neutral DTO. These shims
/// keep the long-standing `pir::plugins::tool_repair::{grammar,repair}::<fn>`
/// signatures (embedders and the integration suites use them) and do the
/// message round-trip in ONE place — [`to_dto`] / [`from_dto`] — so a caller
/// never has to know the crate exists.
///
/// [`AssistantMessage`]: crate::ai::types::AssistantMessage
pub mod api {
    use std::collections::BTreeSet;

    use serde_json::Value as JsonValue;

    use crate::ai::types::AssistantMessage;

    use super::config::GrammarRepairConfig;
    use super::grammar::GrammarRepairResult;
    use super::repair::{MessageRepairOutcome, SchemaCheck};

    /// Engine message → neutral DTO. `None` only for a message that cannot be
    /// represented (never for one this engine produced).
    pub fn to_dto(
        message: &AssistantMessage,
    ) -> Option<pir_builtin_tool_repair::types::AssistantMessage> {
        let value = serde_json::to_value(message).ok()?;
        pir_builtin_tool_repair::types::AssistantMessage::from_json_value(value).ok()
    }

    /// Neutral DTO → engine message, re-attaching the engine-only `deferred`
    /// handle (a repair never touches it, and losing it would strand an
    /// in-flight deferred reply).
    pub fn from_dto(
        dto: pir_builtin_tool_repair::types::AssistantMessage,
        original: &AssistantMessage,
    ) -> Option<AssistantMessage> {
        let mut message: AssistantMessage = serde_json::from_value(dto.to_json_value()).ok()?;
        message.deferred = original.deferred.clone();
        Some(message)
    }

    /// [`super::grammar::repair_message_grammar_leaks`] over an engine message,
    /// in place.
    pub fn repair_message_grammar_leaks(
        message: &mut AssistantMessage,
        config: &GrammarRepairConfig,
        known_tools: &BTreeSet<String>,
    ) -> GrammarRepairResult {
        // The grammar pass reports recovered calls against the DTO's blocks; a
        // caller reads `changed`/`recovered_calls` and inspects the message
        // afterwards, so the pass runs on the DTO and the message is rewritten
        // from it. A message that cannot round-trip is reported unchanged.
        let Some(dto) = to_dto(message) else {
            return GrammarRepairResult::default();
        };
        let mut repaired = dto;
        let result = pir_builtin_tool_repair::grammar::repair_message_grammar_leaks(
            &mut repaired,
            config,
            known_tools,
        );
        if result.changed {
            if let Some(restored) = from_dto(repaired, message) {
                *message = restored;
            }
        }
        result
    }

    /// [`super::repair::repair_message`] over an engine message, in place.
    pub fn repair_message(
        message: &mut AssistantMessage,
        model: Option<&str>,
        tools: &[super::repair::LiveToolSchema],
        check: &dyn Fn(&JsonValue, &JsonValue) -> SchemaCheck,
        debug: bool,
    ) -> MessageRepairOutcome {
        repair_message_with_grammar(
            message,
            model,
            tools,
            &GrammarRepairConfig::default(),
            &BTreeSet::new(),
            check,
            debug,
        )
    }

    /// [`super::repair::repair_message_with_grammar`] over an engine message,
    /// in place. Commits the repaired message only when the pipeline reports a
    /// change, so an unchanged reply keeps its exact bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn repair_message_with_grammar(
        message: &mut AssistantMessage,
        model: Option<&str>,
        tools: &[super::repair::LiveToolSchema],
        grammar_config: &GrammarRepairConfig,
        known_tools: &BTreeSet<String>,
        check: &dyn Fn(&JsonValue, &JsonValue) -> SchemaCheck,
        debug: bool,
    ) -> MessageRepairOutcome {
        let Some(dto) = to_dto(message) else {
            return MessageRepairOutcome::Unchanged;
        };
        let mut repaired = dto;
        let outcome = pir_builtin_tool_repair::repair::repair_message_with_grammar(
            &mut repaired,
            model,
            tools,
            grammar_config,
            known_tools,
            check,
            debug,
        );
        if outcome.changed() {
            if let Some(restored) = from_dto(repaired, message) {
                *message = restored;
            }
        }
        outcome
    }

    /// [`super::repair::normalize_phantom_tool_use`] over an engine message.
    pub fn normalize_phantom_tool_use(message: &mut AssistantMessage) -> bool {
        let Some(dto) = to_dto(message) else {
            return false;
        };
        let mut repaired = dto;
        if !pir_builtin_tool_repair::repair::normalize_phantom_tool_use(&mut repaired) {
            return false;
        }
        match from_dto(repaired, message) {
            Some(restored) => {
                *message = restored;
                true
            }
            None => false,
        }
    }
}

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
        // The name is the crate's (its identity and this host's must match).
        let mut info = PluginInfo::builtin(pir_builtin_tool_repair::TOOL_REPAIR_PLUGIN_NAME);
        // Mirrors the builtin table's `replaceable` flag. Inert for name-based
        // omission (this plugin claims no tool/command/flag names); the
        // native-vs-JS `tool-repair` precedence is decided on the effective
        // extension plan in `plugins::loader`
        // (`loader::tool_repair_precedence`), not here.
        info.replaceable =
            super::builtin_extension_def(pir_builtin_tool_repair::TOOL_REPAIR_PLUGIN_NAME)
                .is_some_and(|def| def.replaceable);
        Self {
            info,
            config: OnceLock::new(),
            model_id: RwLock::new(None),
        }
    }

    /// The lazily loaded config: the `PI_TOOL_REPAIR_DEBUG` env flag plus
    /// `<agent dir>/extensions/pi-tool-repair.json` (package defaults when the
    /// file is absent). The repair hook reads
    /// [`ToolRepairConfig::debug`](config::ToolRepairConfig::debug) from here
    /// and passes it to [`repair::repair_message`].
    ///
    /// The agent-directory policy stays in the engine
    /// ([`crate::ai::providers::agent_dir`]); the crate takes the resolved
    /// directory as a parameter so it needs no environment access.
    pub fn config(&self) -> &config::ToolRepairConfig {
        self.config.get_or_init(|| {
            config::ToolRepairConfig::load_from_agent_dir(&crate::ai::providers::agent_dir())
        })
    }

    /// Whether the Phase 0 `before_provider_request` handler could rewrite a
    /// request body for `model` (the model id the request runs under, when
    /// known).
    ///
    /// The handler gates on the model pushed through
    /// [`AgentPlugin::on_session_context`] and never on the request payload
    /// shape, so this is the exact inertness test the host's hook-install
    /// fast path needs
    /// ([`crate::plugins::host::PluginHost::has_active_handlers_for`]): a
    /// model outside `anchorBleedModels` cannot reach
    /// [`repair::sanitize_request_payload`]'s mutating branch at all.
    ///
    /// Both the caller's model and the cached session model are checked: the
    /// handler always reads the cache, and a summarizer/compaction request can
    /// legitimately run under a different model than the cached session one.
    /// A match on EITHER side answers `true`, which is conservative in the
    /// safe direction — it can keep the value-body path needlessly, but it can
    /// never skip a live handler.
    pub fn is_active_for_model(&self, model: Option<&str>) -> bool {
        // One rule, owned by the crate (its own plugin uses the same function).
        let cached = self
            .model_id
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        pir_builtin_tool_repair::phase0_active_for(model, cached.as_deref())
    }
}

impl AgentPlugin for ToolRepairPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// The one event this plugin consumes: `before_provider_request` (Phase 0).
    /// Declaring the interest is what makes the session/print host consider
    /// installing its per-request transport hook; the host additionally asks
    /// [`AgentPlugin::handler_active_for_event`], so a host whose only
    /// interested plugin is this one skips the hook entirely for a non-gated
    /// model.
    fn events(&self) -> EventInterest {
        EventInterest::List(vec![
            pir_builtin_tool_repair::BEFORE_PROVIDER_REQUEST_EVENT.to_owned()
        ])
    }

    /// The Phase 0 handler is inert for every model outside
    /// `anchorBleedModels` (and before any session-context push), so the host
    /// may skip installing the `before_provider_request` transport hook — and
    /// with it the full `serde_json::Value` request body — when no other
    /// handler is live.
    fn handler_active_for_event(&self, event: &str, model: Option<&str>) -> bool {
        event == pir_builtin_tool_repair::BEFORE_PROVIDER_REQUEST_EVENT
            && self.is_active_for_model(model)
    }

    /// Cache the model id for the Phase 0 gate.
    ///
    /// The snapshot's `model` is either the serialized `Model` object (the
    /// per-prompt push) or the configured model-id string
    /// (`agent_session/runtime_loader.rs:80-84`); both shapes resolve to an id.
    /// A malformed/absent value clears the gate (safe: no repair without a
    /// known model, mirroring the TS `if (!model) return`).
    fn on_session_context(&self, snapshot: &SessionContextSnapshot) {
        // The model-id resolution (serialized model object vs configured
        // model-id string) is the crate's helper: the protocol snapshot and
        // this engine snapshot carry the same `model` shape.
        *self
            .model_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            pir_builtin_tool_repair::model_id_from_snapshot_value(&snapshot.model);
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            if event.name.as_ref() != pir_builtin_tool_repair::BEFORE_PROVIDER_REQUEST_EVENT {
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

    /// Expose the repair config as the host capability: the session/print
    /// `message_end` hook resolves the plugin through this accessor, so a
    /// missing override makes `tool_repair_hook()` always `None` and the
    /// pipeline never runs.
    fn tool_repair_capability(
        &self,
    ) -> Option<&dyn crate::plugins::capabilities::ToolRepairCapability> {
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

/// The `tool-repair` builtin's host capability (see
/// [`crate::plugins::capabilities::ToolRepairCapability`]).
impl crate::plugins::capabilities::ToolRepairCapability for ToolRepairPlugin {
    fn tool_repair_config(&self) -> &config::ToolRepairConfig {
        self.config()
    }
}
