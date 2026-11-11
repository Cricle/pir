//! Live agent events must reach NATIVE Rust plugins, not only the JS runtime.
//!
//! Regression for the phone review's finding 2: the agent loop dispatched
//! `agent_start`, `agent_end`, `message_*`, and `tool_execution_*` only through
//! `LoopDeps::extensions` (the QuickJS handle), gated on
//! `runtime.has_handlers(event)`. In a builtin-only plan the handle is
//! plan-only (no JS runtime, no handlers), so a Rust plugin such as `phone`
//! observed almost nothing.
//!
//! `LoopDeps::plugin_events` now also fans every loop event out to the unified
//! host's non-JS plugins exactly once (the host's JS adapter is skipped), with
//! the JS face still on its direct handle. These tests pin both shapes:
//!
//! 1. a builtin-only `AgentSession` (plan-only handle, no JS) with a recording
//!    Rust plugin sees the documented event set across one scripted
//!    tool-call turn, once per occurrence (terminal events exactly once);
//! 2. with a JS runtime, the same event reaches the Rust plugin once and each
//!    JS handler once — no double dispatch on either face;
//! 3. the SESSION-level events outside the loop (`queue_update`,
//!    `compaction_start`/`_end`, `auto_retry_start`/`_end`, `status`,
//!    `extension_ui_request`) reach a builtin-only session's native plugin
//!    exactly once with their documented payloads;
//! 4. with a JS runtime, each session-level event reaches the native plugin
//!    once and each JS handler once.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::agent::agent_loop::{
    run_with_options, BoxFuture, EventStreamFn, LoopDeps, LoopOptions, ProviderRequest,
};
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::agent_session::{
    AgentSession, AgentSessionConfig, Disposition, EventHandler, SessionEvent, SharedEventStreamFn,
};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Model, ModelCost,
    ModelCostRates, StopReason, StreamOptions, TerminalReason, TextContent, ThinkingLevel,
    ToolCall,
};
use pir::extensions::Notification;
use pir::extensions::{ExtensionConfig, ExtensionHandle, ExtensionLimits};
use pir::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginHost,
    PluginInfo,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-plugin-events-{label}-{}-{}",
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

fn extension_config(cwd: &std::path::Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

fn fake_model() -> Model {
    Model {
        id: "plugin-events-model".into(),
        name: "Plugin events model".into(),
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
        max_tokens: 512,
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

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant_text(text: &str, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "test".into(),
        provider: "test".into(),
        model: "plugin-events-model".into(),
        content: vec![text_block(text)],
        stop_reason,
        timestamp: 3,
        ..AssistantMessage::default()
    }
}

fn assistant_with_tool_call() -> AssistantMessage {
    AssistantMessage {
        api: "test".into(),
        provider: "test".into(),
        model: "plugin-events-model".into(),
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: "call-heartbeat".into(),
            name: "heartbeat".into(),
            arguments: BTreeMap::new(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        stop_reason: StopReason::ToolUse,
        timestamp: 4,
        ..AssistantMessage::default()
    }
}

/// Two scripted provider responses: a tool-call turn followed by a final text
/// turn. Each response streams five events (`start`, `text_start`,
/// `text_delta`, `text_end`, `done`), so `message_update` must be dispatched
/// exactly ten times per run.
fn scripted_batches() -> VecDeque<Vec<AssistantMessageEvent>> {
    let mut empty = assistant_text("", StopReason::Pending);
    empty.content.clear();
    let partial = Arc::new(assistant_text("checking", StopReason::Pending));
    let first = vec![
        AssistantMessageEvent::Start {
            partial: Arc::new(empty.clone()),
        },
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: Arc::new(empty.clone()),
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "checking".into(),
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: "checking".into(),
            partial,
        },
        AssistantMessageEventStream::done_event(
            TerminalReason::ToolUse,
            assistant_with_tool_call(),
        ),
    ];
    let done = Arc::new(assistant_text("done", StopReason::Stop));
    let second = vec![
        AssistantMessageEvent::Start {
            partial: Arc::new(empty.clone()),
        },
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: Arc::new(empty.clone()),
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "done".into(),
            partial: Arc::clone(&done),
        },
        AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: "done".into(),
            partial: Arc::clone(&done),
        },
        AssistantMessageEventStream::done_event(TerminalReason::Stop, (*done).clone()),
    ];
    VecDeque::from(vec![first, second])
}

fn stream_from(events: Vec<AssistantMessageEvent>) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let sink = stream.clone();
    tokio::spawn(async move {
        for event in events {
            sink.push(event);
        }
    });
    stream
}

fn shared_scripted_stream() -> SharedEventStreamFn {
    let batches = Arc::new(Mutex::new(scripted_batches()));
    Arc::new(move |_request: ProviderRequest| {
        let batch = batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_default();
        Box::pin(async move { stream_from(batch) })
    })
}

fn boxed_scripted_stream() -> EventStreamFn {
    let batches = Arc::new(Mutex::new(scripted_batches()));
    Box::new(move |_request: ProviderRequest| {
        let batch = batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_default();
        Box::pin(async move { stream_from(batch) })
    })
}

/// Events recorded by the plugin, shared with the test body.
type RecordedEvents = Arc<Mutex<Vec<(String, Value)>>>;

/// A native plugin recording every event it is handed.
struct RecordingPlugin {
    info: PluginInfo,
    events: Arc<Mutex<Vec<(String, Value)>>>,
    full_message_update_content: bool,
}

impl RecordingPlugin {
    fn new() -> (Self, RecordedEvents) {
        Self::with_message_update_content(false)
    }

    /// A recorder whose `message_update` payloads carry the cumulative
    /// `message.content` (the opt-in native parity mode).
    fn with_full_message_update_content() -> (Self, RecordedEvents) {
        Self::with_message_update_content(true)
    }

    fn with_message_update_content(full_message_update_content: bool) -> (Self, RecordedEvents) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                info: PluginInfo::inline("event-recorder"),
                events: Arc::clone(&events),
                full_message_update_content,
            },
            events,
        )
    }
}

impl AgentPlugin for RecordingPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        // The documented phone set is a subset of All; a wildcard keeps the
        // recorder independent of the list under test.
        EventInterest::All
    }

    fn wants_message_update_content(&self) -> bool {
        self.full_message_update_content
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((event.name.to_string(), event.payload.clone()));
            PluginEventOutcome::None
        })
    }
}

/// A tool that emits two live partial-result snapshots (the shell heartbeat
/// shape the `tool_execution_update` seam exists for).
struct HeartbeatTool;

impl AgentTool for HeartbeatTool {
    fn name(&self) -> &'static str {
        "heartbeat"
    }

    fn description(&self) -> &'static str {
        "Emit two live partial snapshots"
    }

    fn parameters(&self) -> Option<Value> {
        Some(json!({ "type": "object" }))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let on_update = ctx.on_update.clone();
        Box::pin(async move {
            if let Some(on_update) = on_update {
                for (index, text) in ["one", "two"].into_iter().enumerate() {
                    on_update(json!({
                        "toolCallId": "call-heartbeat",
                        "toolName": "heartbeat",
                        "index": index,
                        "content": [{ "type": "text", "text": text }],
                    }));
                }
            }
            ToolResult::ok("heartbeat ok")
        })
    }
}

fn heartbeat_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(HeartbeatTool));
    registry
}

fn event_count(events: &[(String, Value)], name: &str) -> usize {
    events.iter().filter(|(event, _)| event == name).count()
}

async fn wait_for_event(events: &Arc<Mutex<Vec<(String, Value)>>>, name: &str, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let count = {
            let events = events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            event_count(&events, name)
        };
        if count >= expected || Instant::now() >= deadline {
            assert_eq!(count, expected, "`{name}` never reached {expected}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// 1. Builtin-only session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn builtin_only_session_delivers_the_documented_event_set_once() {
    let temp = TempDir::new("builtin-only");
    let (recorder, recorded) = RecordingPlugin::new();

    // The production builtin-only shape: a plan-only extension handle (no
    // QuickJS actor, no JS handlers) whose plan names the native phone plugin.
    let plan: Vec<PathBuf> = if pir::plugins::builtin_names().contains(&"phone") {
        vec![PathBuf::from("builtin:phone")]
    } else {
        Vec::new()
    };
    let handle = ExtensionHandle::start_plan_only(extension_config(&temp.0));
    handle.set_path_plan(plan);
    assert!(!handle.has_js_runtime(), "the plan is builtin-only");

    let session = AgentSession::new_with_plugins(
        AgentSessionConfig {
            performance: Default::default(),
            provider_registry: Arc::new(ProviderRegistry::default()),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: heartbeat_registry(),
            session: None,
            compaction: None,
            max_turns: 4,
            system_prompt: Some("plugin events test".into()),
            cwd: Some(temp.0.clone()),
            event_stream: Some(shared_scripted_stream()),
            extensions: Some(handle),
            tool_selection: None,
        },
        vec![Arc::new(recorder)],
    );

    let handler: EventHandler = Arc::new(|_event| {});
    session
        .prompt("go", handler)
        .await
        .expect("the scripted turn completes");

    // `message_update` is drained by the loop before `message_end`/`agent_end`,
    // so the stream counts are already final. `tool_execution_update` is
    // dispatched fire-and-forget, so wait for both heartbeats.
    wait_for_event(&recorded, "tool_execution_update", 2).await;

    let events = recorded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();

    // One scripted tool-call turn + the final text turn:
    // - agent_start / agent_end exactly once;
    // - tool_execution_start/end exactly once;
    // - tool_execution_update once per heartbeat (2);
    // - message_update once per stream event (5 + 5);
    // - message_start/message_end once per message: the starting user message
    //   (session-routed), the tool-loadout declaration, the first assistant
    //   message, the tool-result message, and the second assistant message
    //   (= 5). The declaration pair is the loop's own `declareToolChanges`.
    let expected: BTreeMap<&str, usize> = BTreeMap::from([
        ("agent_start", 1),
        ("agent_end", 1),
        ("message_start", 5),
        ("message_update", 10),
        ("message_end", 5),
        ("tool_call", 1),
        ("tool_result", 1),
        ("tool_execution_start", 1),
        ("tool_execution_update", 2),
        ("tool_execution_end", 1),
    ]);
    for (name, count) in &expected {
        assert_eq!(
            event_count(&events, name),
            *count,
            "`{name}` reached the native plugin {count} time(s); recorded: {:?}",
            events
                .iter()
                .map(|(event, _)| event.as_str())
                .collect::<Vec<_>>()
        );
    }

    // Payload parity: the native plugin sees the same wire shapes the JS face
    // documents (`type` plus the event-specific fields).
    let start = events
        .iter()
        .find(|(event, _)| event == "tool_execution_start")
        .map(|(_, payload)| payload)
        .expect("tool_execution_start payload");
    assert_eq!(start["type"], json!("tool_execution_start"));
    assert_eq!(start["toolName"], json!("heartbeat"));

    // The gate events reach the native plugin too, once each, with the wire
    // payload the JS face documents (`tool_call` carries the call id, name
    // and input; `tool_result` carries the finalized result fields).
    let gate = events
        .iter()
        .find(|(event, _)| event == "tool_call")
        .map(|(_, payload)| payload)
        .expect("tool_call payload");
    assert_eq!(gate["type"], json!("tool_call"));
    assert_eq!(gate["toolCallId"], json!("call-heartbeat"));
    assert_eq!(gate["toolName"], json!("heartbeat"));
    assert!(
        gate["input"].is_object(),
        "tool_call carries the call input: {gate}"
    );

    let result_event = events
        .iter()
        .find(|(event, _)| event == "tool_result")
        .map(|(_, payload)| payload)
        .expect("tool_result payload");
    assert_eq!(result_event["type"], json!("tool_result"));
    assert_eq!(result_event["toolCallId"], json!("call-heartbeat"));
    assert_eq!(result_event["toolName"], json!("heartbeat"));
    assert_eq!(result_event["isError"], json!(false));
    assert_eq!(result_event["content"][0]["text"], json!("heartbeat ok"));

    let update = events
        .iter()
        .find(|(event, _)| event == "tool_execution_update")
        .map(|(_, payload)| payload)
        .expect("tool_execution_update payload");
    assert_eq!(update["type"], json!("tool_execution_update"));
    assert_eq!(update["toolCallId"], json!("call-heartbeat"));

    let delta = events
        .iter()
        .find(|(event, _)| event == "message_update")
        .map(|(_, payload)| payload)
        .expect("message_update payload");
    assert!(
        delta.get("assistantMessageEvent").is_some(),
        "message_update carries the raw stream event: {delta}"
    );
    // Default (no opt-in) native payloads are the compact O(delta) wire: the
    // hoisted message has scalars only, never the cumulative content.
    for (_, payload) in events.iter().filter(|(event, _)| event == "message_update") {
        assert!(
            payload["message"].get("content").is_none(),
            "default native message_update must omit cumulative content: {payload}"
        );
    }

    let end_payload = events
        .iter()
        .find(|(event, _)| event == "agent_end")
        .map(|(_, payload)| payload)
        .expect("agent_end payload");
    assert!(
        end_payload["messages"]
            .as_array()
            .is_some_and(|m| !m.is_empty()),
        "agent_end carries this run's produced messages: {end_payload}"
    );
}

/// `message_update` content is opt-in for the native face. A plugin that
/// returns `wants_message_update_content() == true` sees the cumulative
/// `message.content` exactly like JS; the default recorder (above) stays on
/// the compact O(delta) wire. This pins the parity mode without moving the
/// default hot path.
#[tokio::test]
async fn native_message_update_full_content_is_opt_in() {
    let temp = TempDir::new("message-update-full");
    let (recorder, recorded) = RecordingPlugin::with_full_message_update_content();
    let handle = ExtensionHandle::start_plan_only(extension_config(&temp.0));

    let session = AgentSession::new_with_plugins(
        AgentSessionConfig {
            performance: Default::default(),
            provider_registry: Arc::new(ProviderRegistry::default()),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: heartbeat_registry(),
            session: None,
            compaction: None,
            max_turns: 4,
            system_prompt: Some("message_update parity test".into()),
            cwd: Some(temp.0.clone()),
            event_stream: Some(shared_scripted_stream()),
            extensions: Some(handle),
            tool_selection: None,
        },
        vec![Arc::new(recorder)],
    );

    let handler: EventHandler = Arc::new(|_event| {});
    session
        .prompt("go", handler)
        .await
        .expect("the scripted turn completes");

    let events = recorded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let delta_with = |fragment: &str| {
        events
            .iter()
            .find(|(event, payload)| {
                event == "message_update"
                    && payload["assistantMessageEvent"]["delta"]
                        .as_str()
                        .is_some_and(|delta| delta == fragment)
            })
            .map(|(_, payload)| payload)
            .unwrap_or_else(|| panic!("missing message_update delta `{fragment}`: {events:?}"))
    };

    let checking = delta_with("checking");
    assert_eq!(
        checking["message"]["content"][0]["text"],
        json!("checking"),
        "opt-in native payload must carry the cumulative message content: {checking}"
    );
    assert_eq!(
        checking["assistantMessageEvent"]["partial"]["content"][0]["text"],
        json!("checking"),
        "the raw stream event keeps its partial snapshot: {checking}"
    );
    let done = delta_with("done");
    assert_eq!(
        done["message"]["content"][0]["text"],
        json!("done"),
        "the second stream's cumulative content: {done}"
    );
}

// ---------------------------------------------------------------------------
// 2. JS runtime + host: one dispatch per face
// ---------------------------------------------------------------------------
#[tokio::test]
async fn js_runtime_and_native_plugins_each_observe_events_once() {
    let temp = TempDir::new("js-and-native");
    let (recorder, recorded) = RecordingPlugin::new();

    // A real QuickJS runtime with one handler per documented loop event:
    // every handler increments its own counter.
    let extension_path = temp.0.join("events.ts");
    std::fs::write(
        &extension_path,
        r#"
        export default function (pi) {
            globalThis.__counts = {};
            for (const name of [
                "agent_start",
                "agent_end",
                "message_start",
                "message_update",
                "message_end",
                "tool_call",
                "tool_result",
                "tool_execution_start",
                "tool_execution_end",
            ]) {
                pi.on(name, () => {
                    globalThis.__counts[name] = (globalThis.__counts[name] ?? 0) + 1;
                });
            }
        }
        "#,
    )
    .expect("write extension");

    let Ok(runtime) = ExtensionHandle::start(extension_config(&temp.0)) else {
        // A build without the `extensions` feature cannot construct a JS
        // runtime; the builtin-only test above still covers the native seam.
        return;
    };
    let loaded = runtime
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load extension");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let host = PluginHost::build(Some(runtime.clone()), vec![Arc::new(recorder)]);
    let deps = LoopDeps {
        tools: Arc::new(heartbeat_registry()),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        extensions: Some(runtime.clone()),
        plugin_events: Some(Arc::new(host)),
        max_turns: 4,
        cwd: temp.0.clone(),
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        event_stream: Some(boxed_scripted_stream()),
        ..LoopOptions::default()
    };
    run_with_options(deps, Context::default(), None, options).await;

    let events = recorded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let probe = runtime
        .eval_json("JSON.stringify(globalThis.__counts)")
        .await
        .expect("probe JS handler counts");
    let js: BTreeMap<String, usize> =
        serde_json::from_value(probe).expect("parse JS handler counts");

    // Loop-direct (no session): the tool-loadout declaration, the assistant
    // message, and the tool-result message per turn, with two streamed
    // responses.
    let expected: BTreeMap<&str, usize> = BTreeMap::from([
        ("agent_start", 1),
        ("agent_end", 1),
        ("message_start", 4),
        ("message_update", 10),
        ("message_end", 4),
        ("tool_call", 1),
        ("tool_result", 1),
        ("tool_execution_start", 1),
        ("tool_execution_end", 1),
    ]);
    for (name, count) in &expected {
        assert_eq!(
            event_count(&events, name),
            *count,
            "native `{name}` must be dispatched exactly {count} time(s): {:?}",
            events
                .iter()
                .map(|(event, _)| event.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            js.get(*name).copied().unwrap_or(0),
            *count,
            "JS `{name}` must be dispatched exactly {count} time(s): {js:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Session-level events outside the loop (builtin-only)
// ---------------------------------------------------------------------------

/// The events the agent-loop enum cannot carry (upstream
/// `AgentSessionEvent`) must reach native plugins too: `queue_update`,
/// `compaction_start`/`_end`, `auto_retry_start`/`_end`, and the
/// notification-shaped `status` / `extension_ui_request` surfaces.
///
/// The queue update is REAL (a steering enqueue emits the mirrored snapshot).
/// The retry and compaction lifecycles are emitted through
/// `emit_session_event_for_test`, the exact `SessionEvent` path the transport
/// retry observers and the compaction hook use — driving a real retryable
/// provider/HTTP transport or a real threshold compaction would need the
/// provider fixtures from `tests/ai_transport_seam.rs`, which this session
/// test deliberately avoids. `status` / `extension_ui_request` go through the
/// notification projection seam (a headless JS runtime produces those
/// notifications in production; a builtin-only plan has no JS engine).
#[tokio::test]
async fn builtin_only_session_delivers_session_level_events_once() {
    let temp = TempDir::new("session-events");
    let (recorder, recorded) = RecordingPlugin::new();

    let plan: Vec<PathBuf> = if pir::plugins::builtin_names().contains(&"phone") {
        vec![PathBuf::from("builtin:phone")]
    } else {
        Vec::new()
    };
    let handle = ExtensionHandle::start_plan_only(extension_config(&temp.0));
    handle.set_path_plan(plan);
    assert!(!handle.has_js_runtime(), "the plan is builtin-only");

    let session = AgentSession::new_with_plugins(
        AgentSessionConfig {
            performance: Default::default(),
            provider_registry: Arc::new(ProviderRegistry::default()),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: heartbeat_registry(),
            session: None,
            compaction: None,
            max_turns: 4,
            system_prompt: Some("plugin events test".into()),
            cwd: Some(temp.0.clone()),
            event_stream: Some(shared_scripted_stream()),
            extensions: Some(handle),
            tool_selection: None,
        },
        vec![Arc::new(recorder)],
    );

    let handler: EventHandler = Arc::new(|_event| {});
    session
        .prompt("go", handler)
        .await
        .expect("the scripted turn completes");

    // A real queue update: the steering enqueue emits the mirrored queues.
    assert_eq!(session.steer("queued steering"), Disposition::Handled);

    // Retry + compaction lifecycles through the session-event emission seam.
    session.emit_session_event_for_test(SessionEvent::AutoRetryStart {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 250,
        error_message: "HTTP 429".into(),
    });
    session.emit_session_event_for_test(SessionEvent::AutoRetryEnd {
        success: true,
        attempt: 1,
        final_error: None,
    });
    session.emit_session_event_for_test(SessionEvent::CompactionStart {
        reason: "threshold",
    });
    session.emit_session_event_for_test(SessionEvent::CompactionEnd {
        reason: "threshold",
        result: Some(json!({ "summary": "compacted" })),
        aborted: false,
        will_retry: false,
        error_message: None,
    });

    // Notification-shaped JS surfaces (`ctx.ui.*`).
    session.dispatch_extension_notification_for_test(&Notification::Status {
        key: "build".into(),
        text: Some("ok".into()),
    });
    session.dispatch_extension_notification_for_test(&Notification::DialogRequest {
        id: 7,
        kind: "confirm".into(),
        title: Some("Continue?".into()),
        options: json!({ "message": "Continue?" }),
    });

    for (name, count) in [
        ("queue_update", 1),
        ("auto_retry_start", 1),
        ("auto_retry_end", 1),
        ("compaction_start", 1),
        ("compaction_end", 1),
        ("status", 1),
        ("extension_ui_request", 1),
    ] {
        wait_for_event(&recorded, name, count).await;
    }

    let events = recorded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for name in [
        "queue_update",
        "auto_retry_start",
        "auto_retry_end",
        "compaction_start",
        "compaction_end",
        "status",
        "extension_ui_request",
    ] {
        assert_eq!(
            event_count(&events, name),
            1,
            "`{name}` must reach the native plugin exactly once: {:?}",
            events
                .iter()
                .map(|(event, _)| event.as_str())
                .collect::<Vec<_>>()
        );
    }
    let payload = |name: &str| {
        events
            .iter()
            .find(|(event, _)| event == name)
            .map(|(_, payload)| payload)
            .unwrap_or_else(|| panic!("`{name}` payload missing"))
    };

    let queue = payload("queue_update");
    assert_eq!(queue["type"], json!("queue_update"));
    assert_eq!(queue["steering"], json!(["queued steering"]));
    assert_eq!(queue["followUp"], json!([]));

    let retry_start = payload("auto_retry_start");
    assert_eq!(retry_start["type"], json!("auto_retry_start"));
    assert_eq!(retry_start["attempt"], json!(1));
    assert_eq!(retry_start["maxAttempts"], json!(3));
    assert_eq!(retry_start["delayMs"], json!(250));
    assert_eq!(retry_start["errorMessage"], json!("HTTP 429"));

    let retry_end = payload("auto_retry_end");
    assert_eq!(retry_end["type"], json!("auto_retry_end"));
    assert_eq!(retry_end["success"], json!(true));
    assert_eq!(retry_end["attempt"], json!(1));

    let compaction_start = payload("compaction_start");
    assert_eq!(compaction_start["type"], json!("compaction_start"));
    assert_eq!(compaction_start["reason"], json!("threshold"));

    let compaction_end = payload("compaction_end");
    assert_eq!(compaction_end["type"], json!("compaction_end"));
    assert_eq!(compaction_end["reason"], json!("threshold"));
    assert_eq!(compaction_end["result"]["summary"], json!("compacted"));
    assert_eq!(compaction_end["aborted"], json!(false));
    assert_eq!(compaction_end["willRetry"], json!(false));

    let status = payload("status");
    assert_eq!(status["type"], json!("status"));
    assert_eq!(status["key"], json!("build"));
    assert_eq!(status["text"], json!("ok"));

    let ui = payload("extension_ui_request");
    assert_eq!(ui["type"], json!("extension_ui_request"));
    assert_eq!(ui["id"], json!("7"));
    assert_eq!(ui["method"], json!("confirm"));
    assert_eq!(ui["title"], json!("Continue?"));
    assert_eq!(ui["message"], json!("Continue?"));
}

// ---------------------------------------------------------------------------
// 4. Session-level events: one copy per face with a JS runtime
// ---------------------------------------------------------------------------

/// With a JS runtime, each session-level event reaches the native plugin once
/// and each JS handler once. The status notification is produced for real by
/// the extension's `session_start` handler (`ctx.ui.setStatus`) and projected
/// by the headless drain; the rest ride the same emission seams as test 3.
#[tokio::test]
async fn js_runtime_and_native_plugins_each_observe_session_events_once() {
    let temp = TempDir::new("js-session-events");
    let (recorder, recorded) = RecordingPlugin::new();

    let extension_path = temp.0.join("session_events.ts");
    std::fs::write(
        &extension_path,
        r#"
        export default function (pi) {
            globalThis.__counts = {};
            for (const name of [
                "queue_update",
                "compaction_start",
                "compaction_end",
                "auto_retry_start",
                "auto_retry_end",
            ]) {
                pi.on(name, () => {
                    globalThis.__counts[name] = (globalThis.__counts[name] ?? 0) + 1;
                });
            }
            pi.on("session_start", async (_event, ctx) => {
                ctx.ui.setStatus("lane", "ready");
            });
        }
        "#,
    )
    .expect("write extension");

    let Ok(runtime) = ExtensionHandle::start(extension_config(&temp.0)) else {
        // A build without the `extensions` feature cannot construct a JS
        // runtime; the builtin-only test above still covers the native seam.
        return;
    };
    let loaded = runtime
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load extension");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let session = AgentSession::new_with_plugins(
        AgentSessionConfig {
            performance: Default::default(),
            provider_registry: Arc::new(ProviderRegistry::default()),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: heartbeat_registry(),
            session: None,
            compaction: None,
            max_turns: 4,
            system_prompt: Some("plugin events test".into()),
            cwd: Some(temp.0.clone()),
            event_stream: Some(shared_scripted_stream()),
            extensions: Some(runtime.clone()),
            tool_selection: None,
        },
        vec![Arc::new(recorder)],
    );

    let handler: EventHandler = Arc::new(|_event| {});
    session
        .prompt("go", handler)
        .await
        .expect("the scripted turn completes");

    // The `session_start` handler's `ctx.ui.setStatus` is a real headless
    // notification: the session mirrors it to the native face.
    wait_for_event(&recorded, "status", 1).await;

    assert_eq!(session.steer("steer me"), Disposition::Handled);
    session.emit_session_event_for_test(SessionEvent::AutoRetryStart {
        attempt: 1,
        max_attempts: 2,
        delay_ms: 10,
        error_message: "HTTP 429".into(),
    });
    session.emit_session_event_for_test(SessionEvent::AutoRetryEnd {
        success: true,
        attempt: 1,
        final_error: None,
    });
    session.emit_session_event_for_test(SessionEvent::CompactionStart {
        reason: "threshold",
    });
    session.emit_session_event_for_test(SessionEvent::CompactionEnd {
        reason: "threshold",
        result: None,
        aborted: false,
        will_retry: false,
        error_message: None,
    });

    let names = [
        "queue_update",
        "auto_retry_start",
        "auto_retry_end",
        "compaction_start",
        "compaction_end",
    ];
    for name in names {
        wait_for_event(&recorded, name, 1).await;
    }

    let events = recorded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for name in names {
        assert_eq!(
            event_count(&events, name),
            1,
            "native `{name}` must be dispatched exactly once: {:?}",
            events
                .iter()
                .map(|(event, _)| event.as_str())
                .collect::<Vec<_>>()
        );
    }

    let probe = runtime
        .eval_json("JSON.stringify(globalThis.__counts)")
        .await
        .expect("probe JS handler counts");
    let js: BTreeMap<String, usize> =
        serde_json::from_value(probe).expect("parse JS handler counts");
    for name in names {
        assert_eq!(
            js.get(name).copied().unwrap_or(0),
            1,
            "JS `{name}` must be dispatched exactly once, no double dispatch: {js:?}"
        );
    }

    // The status projection is native-only: the JS runtime produced the
    // notification (it is not a JS event), so the native face observes it
    // exactly once.
    let status = events
        .iter()
        .find(|(event, _)| event == "status")
        .map(|(_, payload)| payload)
        .expect("status payload");
    assert_eq!(status["key"], json!("lane"));
    assert_eq!(status["text"], json!("ready"));
}

/// Upstream extension ordering (`package-manager.ts:986-1001`,
/// `resource-loader.ts:608-617`): CLI paths first, discovered file paths
/// next, and the builtin extension set LAST — a file extension wins every
/// first-wins conflict (tool/command names, renderer chains, handler order).
#[test]
fn builtin_extensions_load_after_file_extensions() {
    let file_extensions = vec![
        PathBuf::from("extensions/a.ts"),
        PathBuf::from("extensions/b.ts"),
    ];
    let ordered = pir::plugins::order_extension_paths(&[], &file_extensions, false);
    let last_file = ordered
        .iter()
        .rposition(|path| path == &PathBuf::from("extensions/b.ts"))
        .expect("file extension present");
    let first_builtin = ordered
        .iter()
        .position(|path| pir::plugins::builtin_name(&path.to_string_lossy()).is_some())
        .expect("default builtins present");
    assert!(
        last_file < first_builtin,
        "builtins must load after file extensions: {ordered:?}"
    );

    // A CLI path still leads the plan (explicit intent first).
    let ordered =
        pir::plugins::order_extension_paths(&[PathBuf::from("cli.ts")], &file_extensions, false);
    assert_eq!(ordered.first(), Some(&PathBuf::from("cli.ts")));
    assert!(
        ordered
            .iter()
            .position(|path| path == &PathBuf::from("cli.ts"))
            .unwrap()
            < ordered
                .iter()
                .position(|path| pir::plugins::builtin_name(&path.to_string_lossy()).is_some())
                .unwrap(),
        "{ordered:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. `provider_stream_event`: a wedged native handler must not pin the stream
// ---------------------------------------------------------------------------

/// Native plugin that declares `provider_stream_event` and never resolves its
/// handler (no actor-level timeout exists on the native face). `entered`
/// counts handler entries so a test cannot pass on a host whose interest
/// index/emit path never reached the plugin (false green).
struct HangingProviderStreamPlugin {
    info: PluginInfo,
    entered: Arc<AtomicUsize>,
}

impl HangingProviderStreamPlugin {
    fn new(entered: Arc<AtomicUsize>) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline("hanging-provider-stream"),
            entered,
        })
    }
}

impl AgentPlugin for HangingProviderStreamPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["provider_stream_event".to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        if event.name.as_ref() == "provider_stream_event" {
            self.entered.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        } else {
            Box::pin(async { PluginEventOutcome::None })
        }
    }
}

/// Provider that invokes the per-chunk `provider_stream_event` hook from its
/// `stream()` (the same place the SSE parser feeds the sync hook).
struct HookCallingProvider {
    /// Provider events emitted before the terminal `done` event. Each call
    /// blocks on the awaited hook exactly like the SSE feed loop.
    events: usize,
}

impl Provider for HookCallingProvider {
    fn id(&self) -> &str {
        "test"
    }

    fn name(&self) -> &str {
        "Hook-calling test provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let hook = options
            .hooks
            .on_provider_stream_event
            .as_ref()
            .expect("the session must install the provider_stream_event hook");
        for index in 0..self.events {
            hook(&json!({ "probe": "provider_stream_event", "index": index }));
        }
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            assistant_text("done", StopReason::Stop),
        ));
        stream
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
}

/// Build the hanging-provider session for the timeout tests; the returned
/// `TempDir` keeps the cwd alive for the duration of the test.
fn hanging_provider_session(
    label: &str,
    events: usize,
    entered: &Arc<AtomicUsize>,
) -> (Arc<AgentSession>, TempDir) {
    let temp = TempDir::new(label);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(HookCallingProvider { events }));
    let session = AgentSession::new_arc_with_plugins(
        AgentSessionConfig {
            performance: Default::default(),
            provider_registry: Arc::new(registry),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 2,
            system_prompt: Some("provider stream timeout test".into()),
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![HangingProviderStreamPlugin::new(Arc::clone(entered)) as Arc<dyn AgentPlugin>],
    );
    (session, temp)
}

/// Await a spawned prompt within `limit`, aborting it on timeout.
///
/// `session.prompt` blocks INSIDE the synchronous provider hook, so an outer
/// `tokio::time::timeout` over the same future can never preempt it: the
/// blocked task is the one that would have to poll the timer. Spawning moves
/// the prompt to another worker; the JoinHandle then lets a regression fail
/// the timeout assertion instead of hanging the test forever.
async fn prompt_within(
    mut task: tokio::task::JoinHandle<pir::Result<AssistantMessage>>,
    limit: Duration,
    message: &str,
) -> pir::Result<AssistantMessage> {
    match tokio::time::timeout(limit, &mut task).await {
        Ok(joined) => joined.expect("the prompt task must join"),
        Err(_) => {
            task.abort();
            panic!("{message}");
        }
    }
}

/// A native `provider_stream_event` handler returning `pending()` must be cut
/// off by the dispatcher budget: the prompt returns instead of hanging the
/// provider stream forever. The handler-entry assertion also proves the test
/// exercises the timeout path rather than a plan that never dispatches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hanging_native_provider_stream_handler_releases_the_prompt() {
    let entered = Arc::new(AtomicUsize::new(0));
    let (session, _temp) = hanging_provider_session("provider-stream-timeout", 1, &entered);

    // The dispatch budget is aligned with the 30s extension call timeout; the
    // 45s bound exceeds one budget but stays well below two, so both a second
    // dispatch wait and a wedged stream fail the test instead of hanging it.
    let task = tokio::spawn(async move { session.prompt("go", Arc::new(|_event| {})).await });
    let result = prompt_within(
        task,
        Duration::from_secs(45),
        "the prompt must return after the dispatch timeout",
    )
    .await;
    result.expect("the run still completes after the timed-out event");
    assert_eq!(
        entered.load(Ordering::SeqCst),
        1,
        "the hanging handler must actually be entered"
    );
}

/// P2-01: the agent loop's `message_end` dispatch is ONE ordered walk of the
/// host's list (JS adapter at its path-plan slot, inline native plugins last).
/// A native handler's typed replacement is applied to the transcript exactly
/// like a JS one, and the native handler observes the JS handler's replacement
/// in its payload because the host writes each return value back before the
/// next handler.
#[tokio::test]
async fn native_message_end_replacement_is_applied_and_chains_after_js() {
    struct NativeReplacer {
        info: PluginInfo,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl AgentPlugin for NativeReplacer {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["message_end".to_owned()])
        }

        fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            Box::pin(async move {
                let text = event.payload["message"]["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                self.seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(text);
                let mut message = event.payload["message"].clone();
                message["content"] = json!([{ "type": "text", "text": "from-native" }]);
                PluginEventOutcome::Generic(json!({ "message": message }))
            })
        }
    }

    let temp = TempDir::new("p2-01-native-message-end");
    let extension_path = temp.0.join("message-end.ts");
    std::fs::write(
        &extension_path,
        r#"
        export default function (pi) {
            pi.on("message_end", (event) => ({
                message: {
                    ...event.message,
                    content: [{ type: "text", text: "from-js" }],
                },
            }));
        }
        "#,
    )
    .expect("write extension");
    let Ok(runtime) = ExtensionHandle::start(extension_config(&temp.0)) else {
        return;
    };
    let loaded = runtime
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load extension");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = PluginHost::build(
        Some(runtime.clone()),
        vec![Arc::new(NativeReplacer {
            info: PluginInfo::inline("native-message-end-replacer"),
            seen: Arc::clone(&seen),
        }) as Arc<dyn AgentPlugin>],
    );
    let deps = LoopDeps {
        stream: Box::new(|_request| {
            Box::pin(async { assistant_text("original", StopReason::Stop) })
        }),
        tools: Arc::new(ToolRegistry::new()),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        extensions: Some(runtime),
        plugin_events: Some(Arc::new(host)),
        max_turns: 2,
        cwd: temp.0.clone(),
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;

    let assistant = result
        .new_messages
        .iter()
        .find_map(|message| match message {
            pir::ai::types::Message::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .expect("the run produces an assistant message");
    assert_eq!(
        assistant.content,
        vec![text_block("from-native")],
        "the native replacement must be the transcript message"
    );
    assert_eq!(
        *seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec!["from-js".to_owned()],
        "the native handler runs after the JS adapter and sees its replacement"
    );
}

/// P2-01: a native `tool_result` transform is applied to the finalized result
/// through the same hosted dispatch the JS face uses (the scheduler's ordered
/// `tool_result` walk), not dropped on the floor.
#[tokio::test]
async fn native_tool_result_transform_reaches_the_transcript() {
    struct NativeToolResult {
        info: PluginInfo,
    }

    impl AgentPlugin for NativeToolResult {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["tool_result".to_owned()])
        }

        fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            Box::pin(async {
                PluginEventOutcome::ToolResult {
                    content: Some(json!([{ "type": "text", "text": "native-result" }])),
                    details: None,
                    structured_content: None,
                    is_error: Some(false),
                    usage: None,
                }
            })
        }
    }

    let temp = TempDir::new("p2-01-native-tool-result");
    let host = PluginHost::build(
        None,
        vec![Arc::new(NativeToolResult {
            info: PluginInfo::inline("native-tool-result"),
        }) as Arc<dyn AgentPlugin>],
    );
    let deps = LoopDeps {
        stream: Box::new(|_request| Box::pin(async { assistant_with_tool_call() })),
        tools: Arc::new(heartbeat_registry()),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        plugin_events: Some(Arc::new(host)),
        max_turns: 2,
        cwd: temp.0.clone(),
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    let tool_result = result
        .tool_results
        .first()
        .expect("the tool call produces a result message");
    assert_eq!(
        tool_result.content,
        vec![text_block("native-result")],
        "the native tool_result overlay must reach the finalized message"
    );
    assert!(!tool_result.is_error);
}

/// After the first dispatch timeout the dispatcher is closed: a second
/// provider stream event must be dropped before reaching the handler and must
/// not pay another dispatch budget. The prompt therefore returns after ONE
/// budget; a regression that keeps dispatching normally takes two and fails
/// the 45s bound. The entered count (still 1) proves the second event never
/// entered the wedged handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_stream_events_after_a_timeout_are_dropped() {
    let entered = Arc::new(AtomicUsize::new(0));
    let (session, _temp) = hanging_provider_session("provider-stream-timeout-pair", 2, &entered);

    let task = tokio::spawn(async move { session.prompt("go", Arc::new(|_event| {})).await });
    let result = prompt_within(
        task,
        Duration::from_secs(45),
        "the prompt must return after ONE dispatch timeout, not one per event",
    )
    .await;
    result.expect("the run still completes after the timed-out events");
    assert_eq!(
        entered.load(Ordering::SeqCst),
        1,
        "the event after the timeout must be dropped before the handler"
    );
}

/// Build a session whose `provider_stream_event` handler is a REAL JS
/// extension (loaded into an actor-backed handle), so the timeout path runs
/// through the QuickJS interrupt rather than a native future.
async fn js_provider_session(
    label: &str,
    source: &str,
    events: usize,
    call_timeout: Duration,
) -> Option<(Arc<AgentSession>, TempDir, ExtensionHandle)> {
    let temp = TempDir::new(label);
    let entry = temp.0.join("provider-stream.js");
    std::fs::write(&entry, source).expect("write extension fixture");
    let handle = ExtensionHandle::start(ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(ExtensionLimits {
            call_timeout,
            ..Default::default()
        }),
    })
    .ok()?;
    let loaded = handle
        .load(std::slice::from_ref(&entry))
        .await
        .expect("load the JS extension");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(HookCallingProvider { events }));
    let session = AgentSession::new_arc_with_plugins(
        AgentSessionConfig {
            performance: Default::default(),
            provider_registry: Arc::new(registry),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 2,
            system_prompt: Some("js provider stream test".into()),
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: Some(handle.clone()),
            tool_selection: None,
        },
        Vec::new(),
    );
    Some((session, temp, handle))
}

/// C1-02: a `provider_stream_event` handler stuck in a SYNCHRONOUS JS loop is
/// aborted by the QuickJS interrupt (`Ok(Err(js_error))`, not the tokio
/// timeout arm). That outcome must still close the dispatcher: the second
/// provider event is dropped before entering the handler, so the prompt pays
/// one budget total instead of one per event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_js_provider_stream_timeout_closes_the_dispatcher() {
    let source = r#"
export default function (pi) {
    globalThis.__entered = 0;
    pi.on("provider_stream_event", () => {
        globalThis.__entered = (globalThis.__entered ?? 0) + 1;
        const end = Date.now() + 60_000;
        while (Date.now() < end) {}
    });
}
"#;
    let Some((session, _temp, handle)) =
        js_provider_session("js-sync-timeout", source, 2, Duration::from_millis(300)).await
    else {
        return;
    };

    let task = tokio::spawn(async move { session.prompt("go", Arc::new(|_event| {})).await });
    let started = Instant::now();
    let result = prompt_within(
        task,
        Duration::from_secs(10),
        "a synchronous JS loop must be cut off by the dispatch budget",
    )
    .await;
    result.expect("the run still completes after the JS dispatch timeout");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the second event must not pay another budget: {:?}",
        started.elapsed()
    );

    let entered = tokio::time::timeout(
        Duration::from_secs(5),
        handle.eval_json("JSON.stringify(globalThis.__entered ?? 0)"),
    )
    .await
    .expect("probing the JS counter must not hang")
    .expect("probe the JS handler count");
    let entered: u64 = serde_json::from_value(entered).expect("parse the JS handler count");
    assert_eq!(
        entered, 1,
        "the event after the real JS timeout must be dropped before the handler"
    );
}

/// A-06 direction: an ordinary handler error whose TEXT happens to contain
/// "timed out after" must not be mistaken for a dispatcher timeout. Both
/// provider events reach the handler; the prompt does not close the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_error_text_mentioning_timeout_does_not_close_the_dispatcher() {
    let source = r#"
export default function (pi) {
    globalThis.__entered = 0;
    pi.on("provider_stream_event", () => {
        globalThis.__entered = (globalThis.__entered ?? 0) + 1;
        throw new Error("upstream connect timed out after 5s");
    });
}
"#;
    let Some((session, _temp, handle)) =
        js_provider_session("js-timeout-text", source, 2, Duration::from_secs(30)).await
    else {
        return;
    };

    let task = tokio::spawn(async move { session.prompt("go", Arc::new(|_event| {})).await });
    let result = prompt_within(
        task,
        Duration::from_secs(10),
        "ordinary handler errors must not wedge the provider stream",
    )
    .await;
    result.expect("the run completes despite the throwing handler");

    let entered = tokio::time::timeout(
        Duration::from_secs(5),
        handle.eval_json("JSON.stringify(globalThis.__entered ?? 0)"),
    )
    .await
    .expect("probing the JS counter must not hang")
    .expect("probe the JS handler count");
    let entered: u64 = serde_json::from_value(entered).expect("parse the JS handler count");
    assert_eq!(
        entered, 2,
        "an ordinary error mentioning a timeout must not close the dispatcher"
    );
}
