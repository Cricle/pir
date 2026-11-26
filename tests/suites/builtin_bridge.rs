//! The engine ↔ `pir_plugin_api` bridge: identity translation, snapshot
//! projection, and the outcome mapping both directions of the protocol.

use std::sync::{Arc, Mutex};

use pir::plugins::builtin_bridge::{adapt_builtin, outcome_from_protocol};
use pir::plugins::plugin_api as api;
use pir::plugins::{AgentPlugin, PluginEvent, PluginEventOutcome};
use serde_json::json;

/// A protocol plugin that records what the host pushed and returns a fixed
/// outcome.
struct Recorder {
    info: api::PluginInfo,
    snapshots: Mutex<Vec<api::SessionContextSnapshot>>,
    events: Mutex<Vec<String>>,
    outcome: api::PluginEventOutcome,
}

impl Recorder {
    fn new(name: &str, outcome: api::PluginEventOutcome) -> Self {
        let mut info = api::PluginInfo::builtin(name);
        info.replaceable = true;
        Self {
            info,
            snapshots: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            outcome,
        }
    }
}

impl api::BuiltinPlugin for Recorder {
    fn info(&self) -> &api::PluginInfo {
        &self.info
    }

    fn on_session_context(&self, snapshot: &api::SessionContextSnapshot) {
        self.snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(snapshot.clone());
    }

    fn on_event<'a>(&'a self, event: api::PluginEvent<'a>) -> api::PluginFuture<'a> {
        let api::PluginEvent::Named { name, .. } = event;
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(name.to_owned());
        let outcome = self.outcome.clone();
        Box::pin(async move { outcome })
    }
}

#[test]
fn adapter_translates_identity() {
    let recorder = Arc::new(Recorder::new("demo", api::PluginEventOutcome::Continue));
    let plugin = adapt_builtin(recorder);
    assert_eq!(plugin.info().name, "demo");
    assert_eq!(plugin.info().path, "builtin:demo");
    assert!(plugin.info().replaceable);
}

#[test]
fn adapter_projects_the_rich_snapshot() {
    let recorder = Arc::new(Recorder::new("demo", api::PluginEventOutcome::Continue));
    let plugin = adapt_builtin(Arc::clone(&recorder) as Arc<dyn api::BuiltinPlugin>);
    let mut snapshot = pir::extensions::SessionContextSnapshot::default();
    snapshot.model = json!({"id": "gpt-5"});
    snapshot.session_id = Some("sess-1".to_owned());
    snapshot.cwd = Some("/work".to_owned());
    snapshot.project_trusted = true;
    // Fields the protocol does not carry must simply not appear in the
    // projection; the four that do must round-trip exactly.
    plugin.on_session_context(&snapshot);

    let pushed = recorder
        .snapshots
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(pushed.len(), 1);
    assert_eq!(pushed[0].model, json!({"id": "gpt-5"}));
    assert_eq!(pushed[0].session_id.as_deref(), Some("sess-1"));
    assert_eq!(pushed[0].cwd.as_deref(), Some("/work"));
    assert!(pushed[0].project_trusted);
}

#[tokio::test]
async fn adapter_forwards_the_event_name_and_maps_continue_to_none() {
    let recorder = Arc::new(Recorder::new("demo", api::PluginEventOutcome::Continue));
    let plugin = adapt_builtin(Arc::clone(&recorder) as Arc<dyn api::BuiltinPlugin>);
    let mut payload = json!({"toolName": "read"});
    let outcome = plugin
        .on_event(PluginEvent::new("tool_call", &mut payload))
        .await;
    assert!(matches!(outcome, PluginEventOutcome::None));
    assert_eq!(
        recorder
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        ["tool_call"]
    );
}

#[test]
fn continue_maps_to_none_and_handled_maps_per_family() {
    assert!(matches!(
        outcome_from_protocol("tool_call", api::PluginEventOutcome::Continue),
        PluginEventOutcome::None
    ));
    // `input` and `terminal_input` are the two families whose merge contract
    // has a consumer; every other family must not invent a boolean result.
    assert!(matches!(
        outcome_from_protocol("input", api::PluginEventOutcome::Handled),
        PluginEventOutcome::Input {
            action: pir::plugins::InputAction::Handled,
            ..
        }
    ));
    assert!(matches!(
        outcome_from_protocol("terminal_input", api::PluginEventOutcome::Handled),
        PluginEventOutcome::TerminalInput { consume: true, .. }
    ));
    assert!(matches!(
        outcome_from_protocol("tool_call", api::PluginEventOutcome::Handled),
        PluginEventOutcome::None
    ));
}

#[test]
fn replace_is_decoded_with_the_same_wire_rules_as_a_js_result() {
    // The protocol's `Replace` carries a wire result, so the family decides
    // the typed outcome — exactly what a JS handler's return value gets.
    let blocked = outcome_from_protocol(
        "tool_call",
        api::PluginEventOutcome::Replace(json!({"block": true, "reason": "nope"})),
    );
    match blocked {
        PluginEventOutcome::ToolCall {
            block,
            reason,
            terminate,
        } => {
            assert!(block);
            assert_eq!(reason.as_deref(), Some("nope"));
            assert!(!terminate);
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }

    let cancelled = outcome_from_protocol(
        "session_before_switch",
        api::PluginEventOutcome::Replace(json!({"cancel": true})),
    );
    assert!(matches!(
        cancelled,
        PluginEventOutcome::SessionBefore { cancel: true }
    ));
}

#[test]
fn errors_join_messages_and_empty_lists_are_no_opinion() {
    let joined = outcome_from_protocol(
        "tool_call",
        api::PluginEventOutcome::Errors(vec![
            api::EventError {
                message: "first".to_owned(),
            },
            api::EventError {
                message: "second".to_owned(),
            },
        ]),
    );
    match joined {
        PluginEventOutcome::Error { error, timed_out } => {
            assert_eq!(error, "first; second");
            // The protocol cannot express a deadline, so claiming one would be
            // a lie the diagnostics layer would surface as a timeout.
            assert!(!timed_out);
        }
        other => panic!("expected Error, got {other:?}"),
    }
    assert!(matches!(
        outcome_from_protocol("tool_call", api::PluginEventOutcome::Errors(Vec::new())),
        PluginEventOutcome::None
    ));
}
