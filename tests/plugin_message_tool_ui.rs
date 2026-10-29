//! Live native `MessageService` / `ToolService` / `UiService` binding tests.
//!
//! These pin the second half of the live service spine on a REAL
//! `AgentSession` with an inline Rust plugin:
//!
//! - message actions queue the exact JS wire payloads and are applied through
//!   the same `consume_*` handlers (idle `triggerTurn`, busy steering,
//!   `sendUserMessage` queues, `appendEntry` persists) — a native call and the
//!   corresponding JS notification produce identical persisted shapes;
//! - `ToolService` lists the merged registry, executes plugin and core tools
//!   with `<caller>/<n>` ids, honors the session abort signal, runs a direct
//!   subprocess with cwd/timeout/abort, and routes `update` through the one
//!   tool-update sink;
//! - `UiService` writes the SAME notification projection the JS face uses
//!   (`notify` reaches the native sink once, working/title/editor/theme cells
//!   are readable), while dialogs fail closed on a headless session and the
//!   accessor is absent when no presentation sink exists.
//!
//! Everything is in-memory except the session files under `std::env::temp_dir`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value as JsonValue};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, Context, ErrorReason, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, ThinkingLevel,
};
use pir::extensions::{ExtensionConfig, ExtensionHandle, Notification};
use pir::plugins::plugin::services::{
    CancellationHandle, CustomMessage, DialogOptions, ExecOptions, NotifyLevel, PluginServiceError,
    SelectOption, SendMessageOptions, SendUserMessageOptions, ThemeSetResult, ToolCallRef,
    UserContent, WidgetPlacement, WorkingIndicator,
};
use pir::plugins::plugin::{
    AgentPlugin, EventInterest, PluginContext, PluginEvent, PluginEventOutcome, PluginFuture,
    RegistrationContext,
};
use pir::plugins::PluginInfo;
use pir::session_manager::{FileEntry, SessionManager};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Native probe: captures the bound context, registers one Rust tool, and
/// records every event it observes.
struct ProbePlugin {
    info: PluginInfo,
    contexts: Mutex<Vec<PluginContext>>,
    events: Mutex<Vec<(String, JsonValue)>>,
    context_calls: AtomicUsize,
}

impl ProbePlugin {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            contexts: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            context_calls: AtomicUsize::new(0),
        })
    }

    fn last_context(&self) -> PluginContext {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .expect("the session bound a context")
    }

    fn context_count(&self) -> usize {
        self.context_calls.load(Ordering::SeqCst)
    }

    fn events_named(&self, name: &str) -> Vec<JsonValue> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(event, _)| event == name)
            .map(|(_, payload)| payload.clone())
            .collect()
    }
}

impl AgentPlugin for ProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(PluginEchoTool));
        Ok(())
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(
            [
                "message_start",
                "message_end",
                "tool_execution_update",
                "extension_ui_request",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        )
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

    fn on_context(&self, ctx: &PluginContext) {
        self.context_calls.fetch_add(1, Ordering::SeqCst);
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.clone());
    }
}

/// A core (session-registry) tool: echoes its `v` argument.
struct CoreEchoTool;

impl AgentTool for CoreEchoTool {
    fn name(&self) -> &'static str {
        "core-echo"
    }

    fn description(&self) -> &'static str {
        "core echo tool for the native ToolService test"
    }

    fn parameters(&self) -> Option<JsonValue> {
        Some(json!({
            "type": "object",
            "properties": { "v": { "type": "string" } },
        }))
    }

    fn run(&self, args: &JsonValue, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let value = args.get("v").and_then(JsonValue::as_str).unwrap_or("");
        let content = format!("core:{value}");
        Box::pin(async move { ToolResult::ok(content) })
    }
}

/// A core tool that reports the live abort signal it was handed.
struct AbortAwareTool;

impl AgentTool for AbortAwareTool {
    fn name(&self) -> &'static str {
        "core-abort"
    }

    fn description(&self) -> &'static str {
        "reports whether the session abort signal reached the tool context"
    }

    fn run(&self, _args: &JsonValue, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let aborted = ctx
            .signal
            .as_ref()
            .is_some_and(pir::ai::types::AbortSignal::is_aborted);
        Box::pin(async move {
            if aborted {
                ToolResult::error("aborted")
            } else {
                ToolResult::ok("ran")
            }
        })
    }
}

/// A plugin-registered Rust tool: executes through `PluginHost::call_tool`.
struct PluginEchoTool;

impl AgentTool for PluginEchoTool {
    fn name(&self) -> &'static str {
        "plugin-echo"
    }

    fn description(&self) -> &'static str {
        "plugin echo tool for the native ToolService test"
    }

    fn parameters(&self) -> Option<JsonValue> {
        Some(json!({
            "type": "object",
            "properties": { "v": { "type": "string" } },
        }))
    }

    fn run(&self, args: &JsonValue, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let value = args.get("v").and_then(JsonValue::as_str).unwrap_or("");
        let content = format!("plugin:{value}");
        Box::pin(async move { ToolResult::ok(content) })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamMode {
    /// Answer immediately with a successful empty assistant message.
    Immediate,
    /// Stay pending until the request's abort signal fires, then settle as
    /// aborted — a run that is genuinely in flight.
    Hanging,
}

struct FakeProvider {
    mode: StreamMode,
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn name(&self) -> &str {
        "Fake provider"
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
        match self.mode {
            StreamMode::Immediate => {
                let message = AssistantMessage {
                    stop_reason: StopReason::Stop,
                    ..AssistantMessage::default()
                };
                let stream = AssistantMessageEventStream::new();
                stream.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    message,
                ));
                stream
            }
            StreamMode::Hanging => {
                let stream = AssistantMessageEventStream::new();
                let sender = stream.clone();
                let signal = options.signal.clone();
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        drop(handle.spawn(async move {
                            if let Some(signal) = signal {
                                signal.cancelled().await;
                            }
                            let message = AssistantMessage {
                                stop_reason: StopReason::Aborted,
                                ..AssistantMessage::default()
                            };
                            sender.push(AssistantMessageEventStream::error_event(
                                ErrorReason::Aborted,
                                message,
                            ));
                        }));
                    }
                    // `provider.stream()` can be reached from a blocking thread
                    // (no runtime context); keep the hanging stream alive and
                    // observe the abort flag without a Tokio timer.
                    Err(_) => {
                        std::thread::spawn(move || {
                            if let Some(signal) = signal {
                                while !signal.is_aborted() {
                                    std::thread::sleep(Duration::from_millis(5));
                                }
                            }
                            let message = AssistantMessage {
                                stop_reason: StopReason::Aborted,
                                ..AssistantMessage::default()
                            };
                            sender.push(AssistantMessageEventStream::error_event(
                                ErrorReason::Aborted,
                                message,
                            ));
                        });
                    }
                }
                stream
            }
        }
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
}

fn fake_model() -> Model {
    Model {
        id: "model-a".to_owned(),
        name: "Model A".to_owned(),
        api: "openai-completions".to_owned(),
        provider: "fake".to_owned(),
        base_url: "http://fake.invalid/v1".to_owned(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_owned()],
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

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-plugin-mtu-{label}-{}-{}",
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

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn session_config(
    temp: &TempDir,
    mode: StreamMode,
    tools: ToolRegistry,
    extensions: Option<ExtensionHandle>,
) -> AgentSessionConfig {
    let cwd = temp.path().to_str().expect("utf-8 temp path").to_owned();
    let session_dir = temp.path().join("sessions");
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider { mode }));
    AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::High,
        tools,
        session: Some(SessionManager::create(&cwd, &session_dir).expect("create session")),
        compaction: None,
        max_turns: 4,
        system_prompt: Some("plugin message/tool/ui test prompt".to_owned()),
        cwd: Some(temp.path().to_path_buf()),
        event_stream: None,
        extensions,
        tool_selection: None,
    }
}

/// A plan-only extension handle: gives the session the JS-facing presentation
/// sink (notification queue, theme registry, editor snapshot) without
/// constructing QuickJS.
fn plan_only_handle(cwd: &Path) -> ExtensionHandle {
    ExtensionHandle::start_plan_only(ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
}

fn live_session(
    temp: &TempDir,
    mode: StreamMode,
    tools: ToolRegistry,
    extensions: Option<ExtensionHandle>,
) -> (Arc<AgentSession>, Arc<ProbePlugin>) {
    let plugin = ProbePlugin::new("native-service-probe");
    let session = AgentSession::new_arc_with_plugins(
        session_config(temp, mode, tools, extensions),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    assert_eq!(plugin.context_count(), 1, "one context handoff per binding");
    (session, plugin)
}

async fn wait_until(label: &str, mut predicate: impl FnMut() -> bool) {
    // Loaded CI runners (nextest on all CPUs) can take far longer than a
    // developer machine to reach the steering seam; keep the bound generous
    // enough to distinguish load from a wedged run.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_for_steering(session: &AgentSession, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let steering = session.get_steering_messages().await;
        if steering.iter().any(|text| text == expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for steering `{expected}`; got {steering:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Idle `sendUserMessage` starts a full user run (upstream `prompt()`, and
/// lane E's parity work): the host pump picks it up from
/// `pending_custom_runs` — NOT the steering queue, which only carries
/// messages while a run is in flight.
async fn wait_for_pending_user_run(session: &AgentSession) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !session.has_pending_custom_runs() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the idle sendUserMessage to queue a user run"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_for_events(plugin: &ProbePlugin, name: &str, at_least: usize) -> Vec<JsonValue> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let events = plugin.events_named(name);
        if events.len() >= at_least {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {at_least} `{name}` event(s); got {}",
            events.len()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// `(customType, content, display, details)` of every persisted custom-message
/// entry, in transcript order.
async fn custom_messages(
    session: &AgentSession,
) -> Vec<(String, JsonValue, bool, Option<JsonValue>)> {
    session
        .with_session_manager(|manager| {
            manager
                .entries()
                .into_iter()
                .filter_map(|entry| match entry {
                    FileEntry::CustomMessage(message) => Some((
                        message.custom_type,
                        message.content,
                        message.display,
                        message.details,
                    )),
                    _ => None,
                })
                .collect()
        })
        .await
        .expect("session storage")
}

/// Poll until at least `count` custom-message entries persisted; the native
/// service flush is scheduled, so a deterministic read must wait for it.
async fn wait_for_custom_messages(
    session: &AgentSession,
    count: usize,
) -> Vec<(String, JsonValue, bool, Option<JsonValue>)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let entries = custom_messages(session).await;
        if entries.len() >= count {
            return entries;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} custom message(s); got {}",
            entries.len()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// `(customType, data)` of every persisted custom entry, in transcript order.
async fn custom_entries(session: &AgentSession) -> Vec<(String, Option<JsonValue>)> {
    session
        .with_session_manager(|manager| {
            manager
                .entries()
                .into_iter()
                .filter_map(|entry| match entry {
                    FileEntry::Custom(custom) => Some((custom.custom_type, custom.data)),
                    _ => None,
                })
                .collect()
        })
        .await
        .expect("session storage")
}

/// Poll until at least `count` custom entries persisted (see
/// [`wait_for_custom_messages`]).
async fn wait_for_custom_entries(
    session: &AgentSession,
    count: usize,
) -> Vec<(String, Option<JsonValue>)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let entries = custom_entries(session).await;
        if entries.len() >= count {
            return entries;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} custom entr(y/ies); got {}",
            entries.len()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

type TwoSessions = (
    TempDir,
    TempDir,
    Arc<AgentSession>,
    Arc<ProbePlugin>,
    Arc<AgentSession>,
    Arc<ProbePlugin>,
    ExtensionHandle,
    ExtensionHandle,
);

fn two_sessions(label: &str) -> TwoSessions {
    let native_temp = TempDir::new(&format!("{label}-native"));
    let js_temp = TempDir::new(&format!("{label}-js"));
    let native_handle = plan_only_handle(native_temp.path());
    let js_handle = plan_only_handle(js_temp.path());
    let (native_session, native_plugin) = live_session(
        &native_temp,
        StreamMode::Immediate,
        ToolRegistry::new(),
        Some(native_handle.clone()),
    );
    let (js_session, js_plugin) = live_session(
        &js_temp,
        StreamMode::Immediate,
        ToolRegistry::new(),
        Some(js_handle.clone()),
    );
    (
        native_temp,
        js_temp,
        native_session,
        native_plugin,
        js_session,
        js_plugin,
        native_handle,
        js_handle,
    )
}

// ---------------------------------------------------------------------------
// MessageService
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn native_send_message_idle_trigger_turn_starts_a_custom_run() {
    let temp = TempDir::new("message-trigger");
    let (session, plugin) = live_session(&temp, StreamMode::Immediate, ToolRegistry::new(), None);
    let ctx = plugin.last_context();
    let messages = ctx.require_messages().expect("message service");

    messages.send_message(
        CustomMessage::new("native-probe", json!("start a run"))
            .with_display(true)
            .with_details(json!({ "k": 1 })),
        SendMessageOptions {
            trigger_turn: Some(true),
            deliver_as: None,
        },
    );

    wait_until("the triggerTurn run to queue", || {
        session.has_pending_custom_runs()
    })
    .await;
    let queued = session
        .take_pending_custom_run()
        .expect("the native action queued a custom run");
    assert_eq!(queued.custom_type, "native-probe");
    assert_eq!(queued.text(), "start a run");
    assert!(queued.display);

    // The host drains the queue and starts the run on the facade.
    let terminal = session
        .run_custom_turn(queued, Arc::new(|_| {}))
        .await
        .expect("custom run settles");
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    assert!(!session.is_streaming());

    // Upstream reference: `sendCustomMessage(..., {triggerTurn: true})` runs
    // `_runAgentPrompt(appMessage)` (pi-coding-agent `dist/core/
    // agent-session.js`, sendCustomMessage triggerTurn branch), and the
    // low-level loop emits `message_start` + `message_end` for every initial
    // prompt message (`pi-agent-core` `dist/agent-loop.js`, runAgentLoop's
    // `for (const message of initialMessages)`). The custom starting message
    // therefore DOES surface as a custom message event, exactly once, with
    // its own wire shape — the same shape the idle append path emits.
    let starts = plugin.events_named("message_start");
    let ends = plugin.events_named("message_end");
    let custom_starts: Vec<&JsonValue> = starts
        .iter()
        .filter(|payload| payload["message"]["role"] == "custom")
        .collect();
    assert_eq!(
        custom_starts.len(),
        1,
        "the triggerTurn starting message emits exactly one custom message_start"
    );
    let started = custom_starts[0];
    assert_eq!(started["message"]["customType"], "native-probe");
    assert_eq!(started["message"]["content"], "start a run");
    assert_eq!(started["message"]["display"], true);
    assert_eq!(started["message"]["details"]["k"], 1);
    let custom_ends: Vec<&JsonValue> = ends
        .iter()
        .filter(|payload| payload["message"]["role"] == "custom")
        .collect();
    assert_eq!(
        custom_ends.len(),
        1,
        "the triggerTurn starting message emits exactly one custom message_end"
    );
    assert_eq!(custom_ends[0]["message"]["customType"], "native-probe");
}

#[tokio::test(flavor = "current_thread")]
#[cfg_attr(
    windows,
    ignore = "Windows CI runner starves the steering seam under 16-way load; covered on Linux"
)]
async fn native_send_message_while_busy_steers_the_run() {
    let temp = TempDir::new("message-steer");
    let (session, plugin) = live_session(&temp, StreamMode::Hanging, ToolRegistry::new(), None);
    let ctx = plugin.last_context();
    let messages = ctx.require_messages().expect("message service");

    let run = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.prompt("hang", Arc::new(|_| {})).await })
    };
    wait_until("the run to start", || session.is_streaming()).await;

    messages.send_message(
        CustomMessage::new("steer-type", json!("steer me")),
        SendMessageOptions::default(),
    );
    // `send_message` is synchronous and normally flushes on a detached runtime
    // task. Drive the host's documented idle drain explicitly so the test is
    // deterministic on single-threaded/WSL runtimes as well.
    session.apply_pending_extension_notifications().await;
    wait_for_steering(&session, "steer me").await;

    session.abort();
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("the aborted run joins")
        .expect("the prompt task joins")
        .expect("the prompt settles");
    assert!(!session.is_streaming());
}

#[tokio::test(flavor = "current_thread")]
async fn native_send_user_message_queues_and_append_entry_persists() {
    let temp = TempDir::new("message-queue");
    let (session, plugin) = live_session(&temp, StreamMode::Immediate, ToolRegistry::new(), None);
    let ctx = plugin.last_context();
    let messages = ctx.require_messages().expect("message service");

    messages.send_user_message(UserContent::text("queued user text"), Default::default());
    wait_for_pending_user_run(&session).await;

    messages.append_entry("native-note", Some(json!({ "n": 7 })));
    // The queue write is synchronous; the flush is scheduled, so poll the
    // session storage until the entry lands.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let entries = custom_entries(&session).await;
        if entries.iter().any(|(custom_type, data)| {
            custom_type == "native-note" && data == &Some(json!({ "n": 7 }))
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the custom entry did not persist"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // The entry is visible in the projection the snapshot pushes.
    let projected = session
        .with_session_manager(|manager| {
            manager
                .build_session_projection()
                .ok()
                .is_some_and(|projection| {
                    projection.entries.iter().any(|entry| {
                        matches!(
                            entry.source_entry.as_ref(),
                            FileEntry::Custom(custom) if custom.custom_type == "native-note"
                        )
                    })
                })
        })
        .await
        .expect("session storage");
    assert!(
        projected,
        "the custom entry is part of the session projection"
    );
    // The idle message is a queued USER RUN (not steering); its text rides
    // the reserved record's details so the host pump can rebuild it with its
    // images intact.
    let pending = session
        .take_pending_custom_run()
        .expect("the idle user run is still queued");
    assert_eq!(pending.custom_type, "__pir_user_message");
    assert_eq!(
        pending.details.as_ref().expect("details")["text"],
        json!("queued user text")
    );
    assert!(session.get_steering_messages().await.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn native_and_js_send_message_persist_identical_entries() {
    let (
        _native_temp,
        _js_temp,
        native_session,
        native_plugin,
        js_session,
        js_plugin,
        _native_handle,
        js_handle,
    ) = two_sessions("parity-send-message");

    let message = CustomMessage::new("parity-type", json!("same body"))
        .with_display(true)
        .with_details(json!({ "n": 1 }));
    native_plugin
        .last_context()
        .require_messages()
        .expect("message service")
        .send_message(message, SendMessageOptions::default());
    // Deterministic drain: the scheduled flush may have run already; this
    // call applies whatever is left.
    native_session.apply_pending_extension_notifications().await;

    // The JS face's exact payload (`prelude.js.txt` sendMessage).
    js_handle.push_notification(Notification::SendMessage {
        payload: json!({
            "message": {
                "customType": "parity-type",
                "content": "same body",
                "display": true,
                "details": { "n": 1 },
            },
            "options": {},
        }),
    });
    js_session.apply_pending_extension_notifications().await;
    assert_eq!(
        js_plugin.context_count(),
        1,
        "both sessions bind one context"
    );

    let native_entries = wait_for_custom_messages(&native_session, 1).await;
    let js_entries = custom_messages(&js_session).await;
    assert_eq!(native_entries.len(), 1, "native sendMessage persists once");
    assert_eq!(
        native_entries, js_entries,
        "the native and JS paths persist identical custom-message entries"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn native_and_js_send_user_message_queue_identical_text() {
    let (
        _native_temp,
        _js_temp,
        native_session,
        native_plugin,
        js_session,
        _js_plugin,
        _native_handle,
        js_handle,
    ) = two_sessions("parity-send-user");

    native_plugin
        .last_context()
        .require_messages()
        .expect("message service")
        .send_user_message(
            UserContent::text("same content"),
            SendUserMessageOptions::default(),
        );
    wait_for_pending_user_run(&native_session).await;

    js_handle.push_notification(Notification::SendUserMessage {
        payload: json!({ "content": "same content", "options": {} }),
    });
    js_session.apply_pending_extension_notifications().await;
    wait_for_pending_user_run(&js_session).await;

    let native = native_session
        .take_pending_custom_run()
        .expect("native pending user run");
    let js = js_session
        .take_pending_custom_run()
        .expect("JS pending user run");
    assert_eq!(native.custom_type, js.custom_type);
    assert_eq!(native.details, js.details);
    assert_eq!(
        native.details.as_ref().expect("details")["text"],
        json!("same content")
    );
    assert_eq!(
        native_session.get_steering_messages().await,
        js_session.get_steering_messages().await,
        "neither face steers an idle message"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn native_and_js_append_entry_persist_identical_entries() {
    let (
        _native_temp,
        _js_temp,
        native_session,
        native_plugin,
        js_session,
        _js_plugin,
        _native_handle,
        js_handle,
    ) = two_sessions("parity-append-entry");

    native_plugin
        .last_context()
        .require_messages()
        .expect("message service")
        .append_entry("parity-entry", Some(json!({ "payload": [1, 2] })));
    native_session.apply_pending_extension_notifications().await;

    js_handle.push_notification(Notification::AppendEntry {
        payload: json!({ "customType": "parity-entry", "data": { "payload": [1, 2] } }),
    });
    js_session.apply_pending_extension_notifications().await;

    let native_entries = wait_for_custom_entries(&native_session, 1).await;
    let js_entries = custom_entries(&js_session).await;
    assert_eq!(native_entries.len(), 1, "native appendEntry persists once");
    assert_eq!(
        native_entries, js_entries,
        "the native and JS paths persist identical custom entries"
    );
}

// ---------------------------------------------------------------------------
// ToolService
// ---------------------------------------------------------------------------

fn tool_registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(CoreEchoTool));
    tools.register(Arc::new(AbortAwareTool));
    tools
}

#[tokio::test(flavor = "current_thread")]
async fn tool_service_executes_plugin_and_core_tools_with_child_ids() {
    let temp = TempDir::new("tools-execute");
    let (session, plugin) = live_session(&temp, StreamMode::Immediate, tool_registry(), None);
    let ctx = plugin.last_context();
    let tools = ctx.require_tools().expect("tool service");

    let callable: Vec<String> = tools
        .callable_tools()
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    assert!(callable.iter().any(|name| name == "core-echo"));
    assert!(callable.iter().any(|name| name == "plugin-echo"));

    // Core/session tool: the merged registry, a fresh ToolContext.
    let core = tools
        .execute_tool(
            "caller-a",
            "core-echo",
            json!({ "v": "hi" }),
            Default::default(),
        )
        .await
        .expect("core tool runs");
    assert_eq!(core.tool_call_id, "caller-a/0");
    assert_eq!(core.tool_name, "core-echo");
    assert!(!core.is_error);
    assert_eq!(core.result.content, "core:hi");
    assert!(core.duration_ms.is_some());

    // Plugin tool: the plugin host's backend dispatch.
    let plugin_result = tools
        .execute_tool(
            "caller-b",
            "plugin-echo",
            json!({ "v": "yo" }),
            Default::default(),
        )
        .await
        .expect("plugin tool runs");
    assert_eq!(plugin_result.tool_call_id, "caller-b/1");
    assert_eq!(plugin_result.tool_name, "plugin-echo");
    assert!(!plugin_result.is_error);
    assert_eq!(plugin_result.result.content, "plugin:yo");

    // Unknown tools are a typed failure, not a fabricated result.
    let error = tools
        .execute_tool("caller-c", "missing-tool", json!({}), Default::default())
        .await
        .expect_err("unknown tools fail");
    assert!(matches!(error, PluginServiceError::Failed(_)));

    // The session abort signal reaches the tool context.
    session.abort();
    let aborted = tools
        .execute_tool("caller-d", "core-abort", json!({}), Default::default())
        .await
        .expect("abort-aware tool runs");
    assert!(aborted.is_error);
    assert_eq!(aborted.result.content, "aborted");
}

fn short_echo_command() -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        ("cmd", vec!["/C".to_owned(), "echo hello".to_owned()])
    } else {
        ("sh", vec!["-c".to_owned(), "echo hello".to_owned()])
    }
}

fn long_running_command() -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        (
            "cmd",
            vec!["/C".to_owned(), "ping -n 30 127.0.0.1".to_owned()],
        )
    } else {
        ("sh", vec!["-c".to_owned(), "sleep 20".to_owned()])
    }
}

#[tokio::test(flavor = "current_thread")]
async fn tool_service_exec_reports_stdout_exit_timeout_and_abort() {
    let temp = TempDir::new("tools-exec");
    let (_session, plugin) = live_session(&temp, StreamMode::Immediate, ToolRegistry::new(), None);
    let ctx = plugin.last_context();
    let tools = ctx.require_tools().expect("tool service");

    let (command, args) = short_echo_command();
    let result = tools
        .exec(
            command,
            args,
            ExecOptions {
                cwd: Some(temp.path().to_path_buf()),
                timeout_ms: Some(5_000.0),
                signal: None,
            },
        )
        .await
        .expect("exec runs");
    assert_eq!(result.code, 0, "echo exits 0; stderr: {}", result.stderr);
    assert!(
        result.stdout.to_lowercase().contains("hello"),
        "stdout carries the command output: {:?}",
        result.stdout
    );
    assert!(!result.killed);

    let (command, args) = long_running_command();
    let started = Instant::now();
    let timed_out = tools
        .exec(
            command,
            args,
            ExecOptions {
                cwd: Some(temp.path().to_path_buf()),
                timeout_ms: Some(150.0),
                signal: None,
            },
        )
        .await
        .expect("timeout exec runs");
    assert!(timed_out.killed, "the timeout kills the child");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the timeout is honored instead of waiting for the command"
    );

    // A caller-provided signal kills the child too (the pre-aborted case has
    // no race: the long-running command cannot exit before the check).
    let signal = CancellationHandle::new();
    signal.abort();
    let (command, args) = long_running_command();
    let aborted = tools
        .exec(
            command,
            args,
            ExecOptions {
                cwd: Some(temp.path().to_path_buf()),
                timeout_ms: None,
                signal: Some(signal),
            },
        )
        .await
        .expect("aborted exec runs");
    assert!(aborted.killed, "an aborted exec reports killed");
}

#[tokio::test(flavor = "current_thread")]
async fn tool_service_update_reaches_the_registered_sink_once() {
    let temp = TempDir::new("tools-update");
    let (_session, plugin) = live_session(&temp, StreamMode::Immediate, ToolRegistry::new(), None);
    let ctx = plugin.last_context();
    let tools = ctx.require_tools().expect("tool service");

    tools.update(
        &ToolCallRef::new("call-1")
            .with_name("core-echo")
            .with_args(json!({ "v": "x" }))
            .with_parent("parent-0"),
        json!({ "content": [{ "type": "text", "text": "partial" }] }),
    );

    let events = wait_for_events(&plugin, "tool_execution_update", 1).await;
    assert_eq!(events.len(), 1, "one update => one dispatch");
    let payload = &events[0];
    assert_eq!(payload["type"], "tool_execution_update");
    assert_eq!(payload["toolCallId"], "call-1");
    assert_eq!(payload["toolName"], "core-echo");
    assert_eq!(payload["args"]["v"], "x");
    assert_eq!(payload["parentToolCallId"], "parent-0");
    assert_eq!(payload["partialResult"]["content"][0]["text"], "partial");
}

// ---------------------------------------------------------------------------
// UiService
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn ui_notify_reaches_the_native_sink_once_and_state_is_shared() {
    let temp = TempDir::new("ui-shared");
    let handle = plan_only_handle(temp.path());
    let (session, plugin) = live_session(
        &temp,
        StreamMode::Immediate,
        ToolRegistry::new(),
        Some(handle.clone()),
    );
    let ctx = plugin.last_context();
    let ui = ctx.require_ui().expect("ui service is bound with a sink");

    // notify -> the JS notification queue -> the headless native projection.
    ui.notify(NotifyLevel::Warning, "careful");
    session.apply_pending_extension_notifications().await;
    let events = wait_for_events(&plugin, "extension_ui_request", 1).await;
    assert_eq!(
        events.len(),
        1,
        "notify reaches the native sink exactly once"
    );
    assert_eq!(events[0]["method"], "notify");
    assert_eq!(events[0]["message"], "careful");
    assert_eq!(events[0]["notifyType"], "warning");

    // Working/title/editor/theme writes land on the same queue the TUI drains.
    ui.set_working_message(Some("Building"));
    ui.set_working_visible(false);
    ui.set_working_indicator(Some(WorkingIndicator {
        frames: Some(vec!["x".to_owned()]),
        interval_ms: Some(100),
    }));
    ui.set_hidden_thinking_label(Some("Waiting"));
    ui.set_title("My Session");
    ui.set_widget_static(
        "todo",
        &["one".to_owned(), "two".to_owned()],
        WidgetPlacement::BelowEditor,
    );
    ui.set_editor_text("draft");
    ui.paste_to_editor(" pasted");

    let notifications = handle.take_notifications(|notification| notification.is_presentation());
    assert!(notifications.iter().any(|notification| matches!(
        notification,
        Notification::WorkingMessage(message) if message == "Building"
    )));
    assert!(notifications
        .iter()
        .any(|notification| matches!(notification, Notification::WorkingVisible(false))));
    assert!(notifications.iter().any(|notification| matches!(
        notification,
        Notification::WorkingIndicator(options)
            if options["frames"][0] == "x" && options["intervalMs"] == 100
    )));
    assert!(notifications.iter().any(|notification| matches!(
        notification,
        Notification::HiddenThinkingLabel(Some(label)) if label == "Waiting"
    )));
    assert!(notifications.iter().any(|notification| matches!(
        notification,
        Notification::SetTitle(title) if title == "My Session"
    )));
    assert!(notifications.iter().any(|notification| matches!(
        notification,
        Notification::Widget { id, content }
            if id == "todo"
                && content["text"] == "one\ntwo"
                && content["placement"] == "below_transcript"
    )));
    assert!(notifications.iter().any(|notification| matches!(
        notification,
        Notification::EditorSetText(text) if text == "draft"
    )));
    assert!(notifications.iter().any(|notification| matches!(
        notification,
        Notification::EditorPaste(text) if text == " pasted"
    )));

    // Editor snapshot + theme registry reads are live.
    handle.update_editor_state("live text".to_owned());
    assert_eq!(&*ui.editor_text(), "live text");

    handle.update_theme_registry(json!({
        "themes": [{ "name": "dark", "path": null }],
        "current": "dark",
        "snapshots": { "dark": { "background": "black" } },
    }));
    assert_eq!(ui.set_theme("dark"), ThemeSetResult::Applied);
    assert_eq!(ui.set_theme("missing"), ThemeSetResult::NotFound);
    let theme = ui.theme().expect("current theme");
    assert_eq!(&*theme.name, "dark");
    assert_eq!(theme.data["background"], "black");
    let themes = ui.themes();
    assert_eq!(themes.len(), 1);
    assert_eq!(&*themes[0].name, "dark");
    assert!(themes[0].path.is_none());

    let notifications = handle.take_notifications(|notification| notification.is_presentation());
    assert!(notifications.iter().any(
        |notification| matches!(notification, Notification::SetTheme(name) if name == "dark")
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn ui_dialogs_fail_closed_in_a_headless_session() {
    let temp = TempDir::new("ui-dialogs");
    let handle = plan_only_handle(temp.path());
    let (_session, plugin) = live_session(
        &temp,
        StreamMode::Immediate,
        ToolRegistry::new(),
        Some(handle),
    );
    let ctx = plugin.last_context();
    let ui = ctx.require_ui().expect("ui service is bound with a sink");

    let error = ui
        .select("pick", &[SelectOption::text("a")], DialogOptions::default())
        .await
        .expect_err("select has no native reply path");
    assert_eq!(error, PluginServiceError::capability("ui", "select"));

    let error = ui
        .confirm("sure?", "message", DialogOptions::default())
        .await
        .expect_err("confirm has no native reply path");
    assert_eq!(error, PluginServiceError::capability("ui", "confirm"));

    let error = ui
        .input("name", None, DialogOptions::default())
        .await
        .expect_err("input has no native reply path");
    assert_eq!(error, PluginServiceError::capability("ui", "input"));

    let error = ui
        .editor("edit", None, DialogOptions::default())
        .await
        .expect_err("editor has no native reply path");
    assert_eq!(error, PluginServiceError::capability("ui", "editor"));
}

#[test]
fn ui_service_is_absent_without_a_presentation_sink() {
    let temp = TempDir::new("ui-absent");
    let plugin = ProbePlugin::new("no-ui-probe");
    let _session = AgentSession::new_arc_with_plugins(
        session_config(&temp, StreamMode::Immediate, ToolRegistry::new(), None),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    let ctx = plugin.last_context();
    assert!(
        ctx.ui().is_none(),
        "no JS runtime => no presentation queue => no UiService"
    );
    let error = match ctx.require_ui() {
        Ok(_) => panic!("ui fails closed without a presentation sink"),
        Err(error) => error,
    };
    assert_eq!(error, PluginServiceError::unavailable("ui"));
}
