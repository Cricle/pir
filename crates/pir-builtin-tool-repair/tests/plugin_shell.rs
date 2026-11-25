//! Tests for the engine-free plugin shell: identity, the session model gate,
//! and the Phase 0 request helper.

use serde_json::json;

use pir_builtin_tool_repair::ToolRepairPlugin;

use pir_plugin_api::{BuiltinPlugin, PluginInfo, SessionContextSnapshot};

fn snapshot(model: serde_json::Value) -> SessionContextSnapshot {
    SessionContextSnapshot {
        model,
        session_id: None,
        cwd: None,
        project_trusted: false,
    }
}

#[test]
fn the_shell_reports_the_builtin_identity() {
    let plugin = ToolRepairPlugin::new();
    let info: &PluginInfo = plugin.info();
    assert_eq!(info.name, "tool-repair");
    assert_eq!(info.path, "builtin:tool-repair");
    assert!(info.replaceable);
}

#[test]
fn the_session_model_gate_reads_both_snapshot_shapes() {
    let plugin = ToolRepairPlugin::new();
    assert!(plugin.model_id().is_none());
    assert!(!plugin.is_active_for_model(None));

    plugin.on_session_context(&snapshot(json!({"id": "glm-4.6", "name": "GLM"})));
    assert_eq!(plugin.model_id().as_deref(), Some("glm-4.6"));
    assert!(plugin.is_active_for_model(None));
    assert!(plugin.is_active_for_model(Some("claude-3")));

    plugin.on_session_context(&snapshot(json!("kimi-k2")));
    assert_eq!(plugin.model_id().as_deref(), Some("kimi-k2"));

    // A non-gated push clears the gate; the cached model no longer matches.
    plugin.on_session_context(&snapshot(json!("claude-3")));
    assert_eq!(plugin.model_id().as_deref(), Some("claude-3"));
    assert!(!plugin.is_active_for_model(None));

    // A malformed/absent snapshot value clears it too.
    plugin.on_session_context(&snapshot(json!({"name": "no id"})));
    assert!(plugin.model_id().is_none());
    plugin.on_session_context(&snapshot(json!("")));
    assert!(plugin.model_id().is_none());
}

#[test]
fn the_phase0_helper_sanitizes_for_the_cached_model_only() {
    let plugin = ToolRepairPlugin::new();
    let payload = || {
        json!({
            "tools": [{"function": {"parameters": {
                "type": "object",
                "properties": {"path": {"pattern": "^/tmp/"}}
            }}}]
        })
    };

    plugin.on_session_context(&snapshot(json!("glm-4.6")));
    let mut gated = payload();
    assert!(plugin.sanitize_before_provider_request(&mut gated));
    assert_eq!(
        gated["tools"][0]["function"]["parameters"]["properties"]["path"]["pattern"],
        "/tmp/"
    );

    plugin.on_session_context(&snapshot(json!("claude-3")));
    let mut other = payload();
    assert!(!plugin.sanitize_before_provider_request(&mut other));
    assert_eq!(
        other["tools"][0]["function"]["parameters"]["properties"]["path"]["pattern"],
        "^/tmp/"
    );
}
