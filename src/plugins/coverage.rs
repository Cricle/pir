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
//! intentionally not wired goes into [`UNSUPPORTED_NATIVE_EVENTS`] with a
//! reason and the doc's `unsupported-native-events` block instead.

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

/// Whether `event` is declared as reaching the native fan-out.
pub fn is_native_event(event: &str) -> bool {
    WIRED_NATIVE_EVENTS.contains(&event)
}
