//! Canonical native hook-coverage manifest.
//!
//! `docs/hooks.md` carries the human-readable matrix (upstream event → JS face
//! → native face → emit site → parity). The two lists below are the machine
//! half: `tests/hook_coverage.rs` compares the doc's marker blocks against
//! them, so a hook documented as wired but never added here (or removed from
//! here without updating the doc) fails the suite instead of silently
//! regressing.
//!
//! Keep the names exactly in their upstream wire spelling (camelCase events
//! included) — this is the same string every `PluginHost::emit` /
//! `emit_native` call site passes.
//!
//! Update rule for a new hook: wire the emit site, add the name to
//! [`WIRED_NATIVE_EVENTS`], and add the row to the doc's
//! `wired-native-events` block (in the same change). An event that is
//! intentional not wired goes into [`UNSUPPORTED_NATIVE_EVENTS`] with a
//! reason and the doc's `unsupported-native-events` block instead.

use super::plugin::EventInterest;

/// Upstream hook events (plus pir's session-event bridge events) that reach
/// the NATIVE plugin fan-out (`AgentPlugin::on_event`) at least once.
///
/// "Reaches" means a live emit site dispatches the event to the non-JS
/// plugins; for a couple of events (`project_trust`, `ui_prompt_*`) the
/// native face is reached through `PluginHost::emit_native` rather than the
/// mixed `emit`, because the JS face has its own lifecycle path.
pub const WIRED_NATIVE_EVENTS: &[&str] = &[
    // -- trust / resources ------------------------------------------------
    "project_trust",
    "resources_discover",
    "mcp_servers_change",
    // -- session lifecycle ------------------------------------------------
    "session_start",
    "session_info_changed",
    "session_before_switch",
    "session_before_fork",
    "session_before_compact",
    "session_compact",
    "session_compact_failed",
    "session_shutdown",
    "session_before_tree",
    "session_tree",
    // -- context / provider request ---------------------------------------
    "context",
    "context_with_system",
    "cache_warming_decision",
    "before_provider_request",
    "before_provider_headers",
    "after_provider_response",
    "provider_stream_event",
    // -- agent / turn / message -------------------------------------------
    "before_agent_start",
    "agent_start",
    "agent_end",
    "agent_before_settle",
    "agent_settled",
    "turn_start",
    "turn_end",
    "message_start",
    "message_update",
    "message_end",
    // -- tools -------------------------------------------------------------
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
    "tool_call",
    "tool_result",
    // -- interaction -------------------------------------------------------
    "model_select",
    "thinking_level_select",
    "user_bash",
    "input",
    "terminal_input",
    "ui_prompt_start",
    "ui_prompt_end",
    // -- session-event bridge (upstream AgentSessionEvent) ------------------
    "queue_update",
    "compaction_start",
    "compaction_end",
    "auto_retry_start",
    "auto_retry_end",
    "bash_execution_update",
    "background_tool_end",
];

/// Events with no native fan-out, each with the exact reason.
///
/// These are deliberate, documented deviations — not silent omissions. Every
/// upstream `ExtensionEvent` name that is absent from
/// [`WIRED_NATIVE_EVENTS`] must appear here. The list is currently empty:
/// every upstream event reaches the native fan-out.
pub const UNSUPPORTED_NATIVE_EVENTS: &[(&str, &str)] = &[];

/// Native-only observation events that have no upstream `ExtensionAPI.on`
/// counterpart (pir's presentation projection).
pub const NATIVE_ONLY_EVENTS: &[&str] = &["status", "extension_ui_request"];

/// Events that are emitted by pir but are not part of the upstream
/// `ExtensionEvent` union (session-event bridge names and pir synthetic
/// events). `pi.on` accepts them like upstream names, so removing one from
/// this list is a load-time break for extensions observing it.
pub const SYNTHETIC_EVENTS: &[&str] = &[
    // Session-event bridge names (`agent_session/notifications.rs`).
    "entry_appended",
    "thinking_level_changed",
    "summarization_retry_scheduled",
    "summarization_retry_attempt_start",
    "summarization_retry_finished",
    // pir-only run outcome + provider refresh hook.
    "run_failed",
    "provider:refreshModels",
];

/// Synthetic event names used only by in-tree tests through the generic
/// `PluginHost::emit` seam. They stay accepted by `pi.on` so the test harness
/// keeps exercising the same registration path as production extensions.
#[doc(hidden)]
pub const TEST_ONLY_EVENTS: &[&str] = &[
    "gate_probe",
    "probe_event",
    "q3_large_payload",
    "late_event",
];

/// Every event name `pi.on` may register: the wired native fan-out, the
/// native-only presentation projection, pir synthics, and the test-only
/// names. `pi.on` rejects anything outside this set (fail-closed) so a typo
/// cannot silently register a handler that never fires.
pub fn is_known_event(event: &str) -> bool {
    WIRED_NATIVE_EVENTS.contains(&event)
        || NATIVE_ONLY_EVENTS.contains(&event)
        || SYNTHETIC_EVENTS.contains(&event)
        || TEST_ONLY_EVENTS.contains(&event)
}

/// All names accepted by [`is_known_event`], in registry order.
pub fn known_event_names() -> impl Iterator<Item = &'static str> {
    WIRED_NATIVE_EVENTS
        .iter()
        .chain(NATIVE_ONLY_EVENTS.iter())
        .chain(SYNTHETIC_EVENTS.iter())
        .chain(TEST_ONLY_EVENTS.iter())
        .copied()
}

/// Whether `event` is declared as reaching the native fan-out.
pub fn is_native_event(event: &str) -> bool {
    WIRED_NATIVE_EVENTS.contains(&event)
}

/// Names in a native [`EventInterest::List`] that no native emitter can ever
/// deliver.
///
/// [`EventInterest::matches`] is a plain string compare, so a misspelled name
/// silently turns the plugin's `on_event` arm into dead code. This is the
/// validation helper `PluginHost::build` runs over every plugin's declaration
/// (pushing a `PluginDiagnostic` per returned name), so a typo warns at load
/// time instead of leaving an unreachable arm.
///
/// The accepted set is [`is_known_event`] — the SAME registry `pi.on` validates
/// against. That includes the native-only presentation projections
/// ([`NATIVE_ONLY_EVENTS`]), the session-event bridge / pir synthetic names
/// ([`SYNTHETIC_EVENTS`], e.g. `entry_appended` and `run_failed`), and the
/// hidden test-only names: all of them have a native dispatch site, so a
/// plugin may legitimately declare them. `None` / `All` declare no names and
/// always validate.
pub fn invalid_native_event_names(interest: &EventInterest) -> Vec<&str> {
    match interest {
        EventInterest::List(names) => names
            .iter()
            .map(String::as_str)
            .filter(|name| !is_known_event(name))
            .collect(),
        EventInterest::None | EventInterest::All => Vec::new(),
    }
}
