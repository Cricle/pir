//! Native plugin fan-out WIRING (the live `AgentSession` path).
//!
//! `tests/plugin_gates.rs` pins the helpers themselves
//! (`dispatch_user_bash_with_host`, `dispatch_project_trust`, ...). This file
//! pins the production call sites:
//!
//! 1. [`AgentSession::dispatch_user_bash_event`] — the seam the TUI and RPC
//!    `!` paths use — consults a native `user_bash` handler exactly once with
//!    the documented wire payload, even when the session has NO JS runtime
//!    (a builtin-only session that previously executed every `!` command
//!    locally);
//! 2. session construction fans the startup `project_trust` event out to the
//!    native plugins exactly once (the runtime loader already dispatched the
//!    JS face before the host existed);
//! 3. a session built without any plugin surface still constructs and
//!    degrades `user_bash` to local execution (no event, no panic).
//!
//! The fixture is an inline Rust recorder and the session has no extension
//! handle, so no QuickJS runtime (and no `~/.pi` read/write) is involved.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::agent_session::{AgentSession, AgentSessionConfig, UserBashOutcome};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{Model, ModelCost, ModelCostRates, ThinkingLevel};
use pir::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginInfo,
};

type Recorded = Arc<Mutex<Vec<(String, Value)>>>;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-plugin-fanout-wiring-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A native plugin that records every event it observes. `EventInterest::All`
/// keeps it independent of the event list under test (and makes
/// `has_native_handlers` answer true for both events).
struct RecordingPlugin {
    info: PluginInfo,
    seen: Recorded,
}

impl RecordingPlugin {
    fn new() -> (Self, Recorded) {
        let seen: Recorded = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                info: PluginInfo::inline("fanout-wiring-recorder"),
                seen: Arc::clone(&seen),
            },
            seen,
        )
    }
}

impl AgentPlugin for RecordingPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::All
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((event.name.to_string(), event.payload.clone()));
            PluginEventOutcome::None
        })
    }
}

fn fake_model() -> Model {
    Model {
        id: "plugin-fanout-wiring-model".into(),
        name: "Plugin fan-out wiring model".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

/// Builtin-only session shape: no JS runtime, only the inline plugin list.
/// The explicit system prompt keeps context-file discovery (and therefore the
/// agent directory) out of the test.
fn session_config(cwd: PathBuf) -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: Some("plugin fan-out wiring test".into()),
        cwd: Some(cwd),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

fn recorded_events(recorded: &Recorded) -> Vec<(String, Value)> {
    recorded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn event_count(events: &[(String, Value)], name: &str) -> usize {
    events.iter().filter(|(event, _)| event == name).count()
}

/// Bounded poll: `project_trust` is dispatched from a spawned task, so the
/// test waits for the delivery instead of assuming it already ran. The budget
/// stays well under the 10 s test cap.
async fn wait_for_event(recorded: &Recorded, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let count = event_count(&recorded_events(recorded), name);
        if count >= 1 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "`{name}` never reached the native plugin: {:?}",
            recorded_events(recorded)
                .iter()
                .map(|(event, _)| event.as_str())
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// 1. `!` command seam
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_user_bash_event_reaches_the_native_plugin_exactly_once() {
    let temp = TempDir::new("user-bash");
    let (plugin, recorded) = RecordingPlugin::new();
    let session = AgentSession::new_with_plugins(
        session_config(temp.0.clone()),
        vec![Arc::new(plugin) as Arc<dyn AgentPlugin>],
    );
    assert!(
        session.plugin_host().is_some(),
        "inline plugins must build the unified host"
    );

    let outcome = session.dispatch_user_bash_event("echo hi", true).await;
    assert!(
        matches!(outcome, UserBashOutcome::Local),
        "a non-deciding native handler keeps local execution"
    );

    let events = recorded_events(&recorded);
    assert_eq!(
        event_count(&events, "user_bash"),
        1,
        "the native plugin observes `user_bash` exactly once: {:?}",
        events
            .iter()
            .map(|(event, _)| event.as_str())
            .collect::<Vec<_>>()
    );
    let payload = events
        .iter()
        .find(|(event, _)| event == "user_bash")
        .map(|(_, payload)| payload)
        .expect("user_bash payload");
    assert_eq!(payload["type"], json!("user_bash"));
    assert_eq!(payload["command"], json!("echo hi"));
    assert_eq!(
        payload["excludeFromContext"],
        json!(true),
        "the `!!` flag travels on the wire payload"
    );
    assert!(
        payload["cwd"].is_string(),
        "the payload carries the session cwd display form: {payload}"
    );
}

// ---------------------------------------------------------------------------
// 2. Startup `project_trust`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_construction_dispatches_project_trust_to_native_plugins_once() {
    let temp = TempDir::new("project-trust");
    let (plugin, recorded) = RecordingPlugin::new();
    let _session = AgentSession::new_with_plugins(
        session_config(temp.0.clone()),
        vec![Arc::new(plugin) as Arc<dyn AgentPlugin>],
    );

    // Construction spawns the native fan-out; await its delivery.
    wait_for_event(&recorded, "project_trust").await;
    let events = recorded_events(&recorded);
    assert_eq!(
        event_count(&events, "project_trust"),
        1,
        "the startup `project_trust` event reaches the native plugin exactly once"
    );
    let payload = events
        .iter()
        .find(|(event, _)| event == "project_trust")
        .map(|(_, payload)| payload)
        .expect("project_trust payload");
    assert_eq!(payload["type"], json!("project_trust"));
    assert_eq!(
        payload["cwd"],
        json!(temp.0.display().to_string()),
        "the payload cwd is the session config cwd in display form"
    );
}

// ---------------------------------------------------------------------------
// 3. No plugin surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_without_plugins_builds_and_degrades_to_local() {
    let temp = TempDir::new("no-plugins");
    // No extension handle, no inline plugins: construction must not panic and
    // must not build a host (the startup `project_trust` fan-out is skipped).
    let session = AgentSession::new_with_plugins(session_config(temp.0.clone()), Vec::new());
    assert!(
        session.plugin_host().is_none(),
        "a session with no plugin surface has no plugin host"
    );
    let outcome = session.dispatch_user_bash_event("echo hi", false).await;
    assert!(
        matches!(outcome, UserBashOutcome::Local),
        "with no handler the command runs locally"
    );
}
