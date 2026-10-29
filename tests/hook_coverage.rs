//! Hook-coverage gates for the native plugin surface.
//!
//! Two kinds of checks live here:
//!
//! 1. **Documentation gate** — `docs/hooks.md` declares the wired and
//!    unsupported native events in marker blocks. Those blocks must match
//!    `pir::plugins::coverage` exactly, and every upstream `ExtensionEvent`
//!    name must be accounted for (wired or explicitly unsupported). A hook
//!    that is added to the dispatcher without updating the doc (or removed
//!    from the doc without removing the wiring) fails here instead of
//!    silently regressing.
//! 2. **Live dispatch** — the newly wired fan-outs, each observed exactly
//!    once by a native recorder in a builtin-only plan and, where a JS face
//!    exists, alongside a real QuickJS runtime with a JS handler:
//!    `ui_prompt_start`/`_end` around native dialogs (ok/cancel/timeout/drop,
//!    and not double-emitted for JS dialogs), `session_shutdown` before
//!    `on_shutdown`, `context` / `context_with_system`, print's
//!    `agent_settled` / `provider_stream_event`, `mcp_servers_change` from
//!    the MCP registry rebuild (JS keeps its direct-handle copy, native gets
//!    one `emit_native` copy), and `session_before_tree` (cancel keeps the
//!    leaf and skips summarization on both faces).
//!
//! All fixtures are in-memory plus a session file under `std::env::temp_dir`.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::agent::agent_loop::{run_with_options, EventStreamFn, LoopDeps, LoopOptions};
use pir::agent::tool_execution::ToolRegistry;
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, ModelCost,
    ModelCostRates, StopReason, TerminalReason, TextContent, ThinkingLevel, UserMessage, UserRole,
};
use pir::coding_agent::print::{dispatch_print_agent_settled, print_transport_hooks_with_model};
use pir::extensions::{ExtensionConfig, ExtensionHandle, Notification};
use pir::plugins::coverage::{UNSUPPORTED_NATIVE_EVENTS, WIRED_NATIVE_EVENTS};
use pir::plugins::plugin::PluginContext;
use pir::plugins::services::{DialogOptions, SelectOption, TerminalInputAction, UiService};
use pir::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginHost,
    PluginInfo,
};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Documentation gate
// ---------------------------------------------------------------------------

fn hooks_doc() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/hooks.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The events of one `<!-- marker --> ... <!-- /marker -->` block, in file
/// order. Blank lines are ignored; each remaining line's first whitespace
/// token is the event name.
fn doc_marker_events(marker: &str) -> Vec<String> {
    let doc = hooks_doc();
    let open = format!("<!-- {marker} -->");
    let close = format!("<!-- /{marker} -->");
    let body = doc
        .split_once(&open)
        .unwrap_or_else(|| panic!("docs/hooks.md is missing `{open}`"))
        .1
        .split_once(&close)
        .unwrap_or_else(|| panic!("docs/hooks.md is missing `{close}`"))
        .0;
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split_whitespace()
                .next()
                .expect("a non-empty marker line")
                .to_owned()
        })
        .collect()
}

#[test]
fn doc_wired_block_matches_the_dispatch_registry() {
    let mut doc = doc_marker_events("wired-native-events");
    let mut registry: Vec<String> = WIRED_NATIVE_EVENTS
        .iter()
        .map(|event| (*event).to_owned())
        .collect();
    doc.sort();
    registry.sort();
    assert_eq!(
        doc, registry,
        "docs/hooks.md `wired-native-events` must list exactly \
         src/plugins/coverage.rs::WIRED_NATIVE_EVENTS"
    );
}

#[test]
fn doc_unsupported_block_matches_the_registry() {
    let mut doc = doc_marker_events("unsupported-native-events");
    let mut registry: Vec<String> = UNSUPPORTED_NATIVE_EVENTS
        .iter()
        .map(|(event, _)| (*event).to_owned())
        .collect();
    doc.sort();
    registry.sort();
    assert_eq!(
        doc, registry,
        "docs/hooks.md `unsupported-native-events` must list exactly \
         src/plugins/coverage.rs::UNSUPPORTED_NATIVE_EVENTS"
    );
}

#[test]
fn every_upstream_extension_event_is_accounted_for() {
    // upstream `ExtensionEvent` / `ExtensionAPI.on` names
    // (dist/core/extensions/types.d.ts). Keep this list in upstream spelling.
    const UPSTREAM: &[&str] = &[
        "project_trust",
        "resources_discover",
        "mcp_servers_change",
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
        "context",
        "context_with_system",
        "cache_warming_decision",
        "before_provider_request",
        "before_provider_headers",
        "after_provider_response",
        "provider_stream_event",
        "before_agent_start",
        "agent_start",
        "agent_end",
        "agent_before_settle",
        "agent_settled",
        "ui_prompt_start",
        "ui_prompt_end",
        "turn_start",
        "turn_end",
        "message_start",
        "message_update",
        "message_end",
        "tool_execution_start",
        "tool_execution_update",
        "tool_execution_end",
        "model_select",
        "thinking_level_select",
        "tool_call",
        "tool_result",
        "user_bash",
        "input",
    ];
    let unsupported: Vec<&str> = UNSUPPORTED_NATIVE_EVENTS
        .iter()
        .map(|(event, _)| *event)
        .collect();
    for event in UPSTREAM {
        assert!(
            WIRED_NATIVE_EVENTS.contains(event) || unsupported.contains(event),
            "upstream hook `{event}` is neither wired natively nor documented as \
             unsupported in src/plugins/coverage.rs"
        );
    }
    for event in WIRED_NATIVE_EVENTS {
        assert!(
            !unsupported.contains(event),
            "`{event}` is listed as both wired and unsupported"
        );
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-hook-coverage-{label}-{}-{}",
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

type Recorded = Arc<Mutex<Vec<(String, Value)>>>;

/// Native plugin recording every event.
struct EventRecorder {
    info: PluginInfo,
    events: Recorded,
    contexts: Mutex<Vec<PluginContext>>,
}

impl EventRecorder {
    fn new() -> (Arc<Self>, Recorded) {
        let events: Recorded = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                info: PluginInfo::inline("hook-coverage-recorder"),
                events: Arc::clone(&events),
                contexts: Mutex::new(Vec::new()),
            }),
            events,
        )
    }

    fn last_context(&self) -> PluginContext {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .expect("the session bound a context")
    }
}

impl AgentPlugin for EventRecorder {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::All
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
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.clone());
    }
}

fn fake_model() -> Model {
    Model {
        id: "hook-coverage-model".to_owned(),
        name: "Hook coverage model".to_owned(),
        api: "openai-completions".to_owned(),
        provider: "hook-fake".to_owned(),
        base_url: "http://fake.invalid/v1".to_owned(),
        reasoning: false,
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

fn extension_config(cwd: &Path, has_ui: bool) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: if has_ui { "interactive" } else { "headless" }.to_owned(),
        has_ui,
        model: Some("hook-coverage-model".to_owned()),
        session_id: Some("hook-coverage".to_owned()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

fn session_config(cwd: &Path, handle: &ExtensionHandle) -> AgentSessionConfig {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider));
    AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: Some("hook coverage".to_owned()),
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: Some(handle.clone()),
        tool_selection: None,
    }
}

struct FakeProvider;

impl pir::ai::provider::Provider for FakeProvider {
    fn id(&self) -> &str {
        "hook-fake"
    }

    fn name(&self) -> &str {
        "Hook coverage fake"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: pir::ai::types::StreamOptions,
    ) -> AssistantMessageEventStream {
        AssistantMessageEventStream::new()
    }

    fn api_key_auth(&self) -> Vec<pir::ai::auth::ApiKeyAuth> {
        Vec::new()
    }
}

fn event_count(events: &Recorded, name: &str) -> usize {
    events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(event, _)| event == name)
        .count()
}

async fn wait_for_count(events: &Recorded, name: &str, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let count = event_count(events, name);
        if count >= expected || Instant::now() >= deadline {
            assert_eq!(count, expected, "`{name}` never reached {expected}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn payloads(events: &Recorded, name: &str) -> Vec<Value> {
    events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(event, _)| event == name)
        .map(|(_, payload)| payload.clone())
        .collect()
}

struct NativeDialogFixture {
    _temp: TempDir,
    session: Arc<AgentSession>,
    handle: ExtensionHandle,
    recorder: Arc<EventRecorder>,
    events: Recorded,
}

impl NativeDialogFixture {
    fn new(label: &str) -> Self {
        let temp = TempDir::new(label);
        let handle = ExtensionHandle::start_plan_only(extension_config(temp.0.as_path(), true));
        let (recorder, events) = EventRecorder::new();
        let session = AgentSession::new_arc_with_plugins(
            session_config(&temp.0, &handle),
            vec![Arc::clone(&recorder) as Arc<dyn AgentPlugin>],
        );
        Self {
            _temp: temp,
            session,
            handle,
            recorder,
            events,
        }
    }

    fn ui(&self) -> Arc<dyn UiService> {
        self.recorder
            .last_context()
            .ui_arc()
            .expect("ui service is bound")
    }

    fn requests(&self) -> Vec<(u64, String, Value)> {
        self.handle
            .take_notifications(|notification| {
                matches!(notification, Notification::DialogRequest { .. })
            })
            .into_iter()
            .filter_map(|notification| match notification {
                Notification::DialogRequest {
                    id, kind, options, ..
                } => Some((id, kind, options)),
                _ => None,
            })
            .collect()
    }

    fn respond(&self, id: u64, value: Value) -> bool {
        self.session
            .plugin_host()
            .expect("session plugin host")
            .native_dialogs()
            .respond(id, value)
    }
}

// ---------------------------------------------------------------------------
// Native dialogs: ui_prompt_start / ui_prompt_end
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn native_dialog_ok_emits_start_then_end_once() {
    let fixture = NativeDialogFixture::new("dialog-ok");
    let ui = fixture.ui();

    let pending = ui.select(
        "Pick one",
        &[SelectOption::new("First", "a")],
        DialogOptions::default(),
    );
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1, "one request per call");
    assert_eq!(requests[0].1, "select");
    assert!(fixture.respond(requests[0].0, json!("a")));
    assert_eq!(
        pending.await.expect("the select resolves"),
        Some("a".to_owned())
    );

    assert_eq!(
        payloads(&fixture.events, "ui_prompt_start"),
        vec![json!({
            "type": "ui_prompt_start",
            "reason": "ui_prompt",
            "kind": "select",
            "title": "Pick one",
        })]
    );
    assert_eq!(
        payloads(&fixture.events, "ui_prompt_end"),
        vec![json!({
            "type": "ui_prompt_end",
            "reason": "ui_prompt",
            "kind": "select",
            "title": "Pick one",
        })]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn native_dialog_cancel_emits_end_once() {
    let fixture = NativeDialogFixture::new("dialog-cancel");
    let ui = fixture.ui();

    let pending = ui.confirm("Proceed?", "sure?", DialogOptions::default());
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert!(fixture.respond(requests[0].0, Value::Null));
    assert_eq!(
        pending.await.expect("the confirm resolves"),
        None,
        "cancel maps to None"
    );
    assert_eq!(event_count(&fixture.events, "ui_prompt_start"), 1);
    assert_eq!(event_count(&fixture.events, "ui_prompt_end"), 1);
    assert_eq!(
        payloads(&fixture.events, "ui_prompt_end")[0]["kind"],
        json!("confirm")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn native_dialog_timeout_emits_end_once() {
    let fixture = NativeDialogFixture::new("dialog-timeout");
    let ui = fixture.ui();

    let error = ui
        .input(
            "Your name",
            None,
            DialogOptions::with_timeout(Duration::from_millis(40)),
        )
        .await
        .expect_err("an unanswered dialog times out");
    assert!(error.to_string().contains("timed out"), "{error}");
    assert_eq!(event_count(&fixture.events, "ui_prompt_start"), 1);
    assert_eq!(event_count(&fixture.events, "ui_prompt_end"), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_native_dialog_emits_end_once() {
    let fixture = NativeDialogFixture::new("dialog-drop");
    let ui = fixture.ui();

    let mut pending = Box::pin(ui.editor("Compose", None, DialogOptions::default()));
    // Drive the first poll: the start event is emitted before the wait parks.
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    assert!(matches!(pending.as_mut().poll(&mut cx), Poll::Pending));
    assert_eq!(
        event_count(&fixture.events, "ui_prompt_start"),
        1,
        "start fires on the first poll"
    );
    drop(pending);

    wait_for_count(&fixture.events, "ui_prompt_end", 1).await;
    assert_eq!(event_count(&fixture.events, "ui_prompt_end"), 1);
    assert_eq!(
        payloads(&fixture.events, "ui_prompt_end")[0]["kind"],
        json!("editor")
    );
}

/// A JS-face dialog keeps its prelude pair on the JS face: the native
/// recorder must not observe a second copy, and the native dialog path must
/// not invoke the JS handlers either.
#[tokio::test(flavor = "current_thread")]
async fn js_dialogs_are_not_double_emitted() {
    let temp = TempDir::new("js-dialog");
    let extension_path = temp.0.join("dialogs.ts");
    std::fs::write(
        &extension_path,
        r#"
        export default function (pi) {
            globalThis.__promptEvents = [];
            pi.on("ui_prompt_start", (event) => {
                globalThis.__promptEvents.push(event.type);
            });
            pi.on("ui_prompt_end", (event) => {
                globalThis.__promptEvents.push(event.type);
            });
            pi.registerTool({
                name: "js_ask",
                description: "one JS dialog",
                parameters: { type: "object" },
                async execute(_id, _params, _signal, _onUpdate, ctx) {
                    const answer = await ctx.ui.select("JS pick", ["a"]);
                    return { content: [{ type: "text", text: String(answer) }] };
                },
            });
        }
        "#,
    )
    .expect("write extension");
    let Ok(handle) = ExtensionHandle::start(extension_config(&temp.0, true)) else {
        return; // build without the extensions feature
    };
    let loaded = handle
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let (recorder, events) = EventRecorder::new();
    let session = AgentSession::new_arc_with_plugins(
        session_config(&temp.0, &handle),
        vec![Arc::clone(&recorder) as Arc<dyn AgentPlugin>],
    );

    // A native dialog: native events fire, JS handlers stay untouched.
    let ui = recorder
        .last_context()
        .ui_arc()
        .expect("ui service is bound");
    let pending = ui.select(
        "Native pick",
        &[SelectOption::text("a")],
        DialogOptions::default(),
    );
    let request = handle
        .take_notifications(|notification| {
            matches!(notification, Notification::DialogRequest { .. })
        })
        .into_iter()
        .find_map(|notification| match notification {
            Notification::DialogRequest { id, .. } => Some(id),
            _ => None,
        })
        .expect("native dialog request");
    assert!(session
        .plugin_host()
        .expect("host")
        .native_dialogs()
        .respond(request, json!("a")));
    pending.await.expect("native dialog resolves");
    assert_eq!(event_count(&events, "ui_prompt_start"), 1);
    let js_after_native: Value = handle
        .eval_json("JSON.stringify(globalThis.__promptEvents)")
        .await
        .expect("eval JS prompt events");
    assert_eq!(
        js_after_native,
        json!([]),
        "native dialogs must not dispatch ui_prompt_* to the JS handlers"
    );

    // A JS dialog: the prelude pair reaches the JS handler, and the native
    // recorder still observes nothing for it.
    let worker = tokio::spawn({
        let handle = handle.clone();
        async move { handle.call_tool("js_ask", "d-js", &json!({})).await }
    });
    let mut js_request = None;
    for _ in 0..400 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if let Some(Notification::DialogRequest { id, .. }) = handle
            .take_notifications(|notification| {
                matches!(notification, Notification::DialogRequest { .. })
            })
            .into_iter()
            .last()
        {
            js_request = Some(id);
            break;
        }
    }
    let id = js_request.expect("JS dialog request");
    handle.respond_dialog(&id.to_string(), json!("a"));
    worker
        .await
        .expect("worker joins")
        .expect("JS tool call completes");
    assert_eq!(
        event_count(&events, "ui_prompt_start"),
        1,
        "the JS dialog added no native ui_prompt_start"
    );
    assert_eq!(
        event_count(&events, "ui_prompt_end"),
        1,
        "the JS dialog added no native ui_prompt_end"
    );
    let js_events: Value = handle
        .eval_json("JSON.stringify(globalThis.__promptEvents)")
        .await
        .expect("eval JS prompt events");
    assert_eq!(
        js_events,
        json!(["ui_prompt_start", "ui_prompt_end"]),
        "the JS face emits its own pair exactly once"
    );
}

// ---------------------------------------------------------------------------
// session_shutdown
// ---------------------------------------------------------------------------

/// A recorder that also traces `on_shutdown`, so ordering can be asserted.
struct ShutdownRecorder {
    info: PluginInfo,
    trace: Arc<Mutex<Vec<String>>>,
}

impl AgentPlugin for ShutdownRecorder {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["session_shutdown".to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            let reason = event
                .payload
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            self.trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("event:{reason}"));
            PluginEventOutcome::None
        })
    }

    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async {
            self.trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("on_shutdown".to_owned());
            Ok(())
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn session_shutdown_precedes_on_shutdown_and_is_idempotent() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::new(ShutdownRecorder {
        info: PluginInfo::inline("shutdown-recorder"),
        trace: Arc::clone(&trace),
    });
    let host = PluginHost::build(None, vec![recorder as Arc<dyn AgentPlugin>]);

    host.shutdown().await;
    assert_eq!(
        *trace
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec!["event:quit".to_owned(), "on_shutdown".to_owned()],
        "upstream session_shutdown fires before on_shutdown"
    );

    // A second teardown (session teardown racing a child run) is a no-op.
    host.shutdown().await;
    assert_eq!(trace.lock().unwrap().len(), 2, "shutdown is exactly-once");
}

#[tokio::test(flavor = "current_thread")]
async fn host_shutdown_dispatches_session_shutdown_to_the_js_face_once_and_stops_the_actor() {
    let temp = TempDir::new("shutdown-js");
    let marker = temp.0.join("shutdown.txt");
    let extension_path = temp.0.join("shutdown.ts");
    let marker_js = serde_json::to_string(&marker.to_string_lossy().to_string())
        .expect("marker path as a JS string");
    std::fs::write(
        &extension_path,
        format!(
            r#"
        import fs from "node:fs";
        export default function (pi) {{
            pi.on("session_shutdown", (event) => {{
                fs.appendFileSync({marker_js}, event.reason + "\n");
            }});
        }}
        "#
        ),
    )
    .expect("write extension");
    let Ok(runtime) = ExtensionHandle::start(extension_config(&temp.0, false)) else {
        return;
    };
    let loaded = runtime
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let trace = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::new(ShutdownRecorder {
        info: PluginInfo::inline("shutdown-native"),
        trace: Arc::clone(&trace),
    });
    let host = PluginHost::build(
        Some(runtime.clone()),
        vec![recorder as Arc<dyn AgentPlugin>],
    );

    host.shutdown().await;
    assert_eq!(
        *trace.lock().unwrap(),
        vec!["event:quit".to_owned(), "on_shutdown".to_owned()],
        "the native face sees quit exactly once"
    );
    assert_eq!(
        std::fs::read_to_string(&marker).expect("JS session_shutdown handler ran"),
        "quit\n",
        "the JS face receives session_shutdown {{reason: \"quit\"}} exactly once"
    );

    // A second teardown (session teardown racing a child run) is a no-op.
    host.shutdown().await;
    assert_eq!(
        std::fs::read_to_string(&marker).expect("marker survives the second teardown"),
        "quit\n",
        "a second host shutdown must not re-dispatch the JS event"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.is_ready() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !runtime.is_ready(),
        "host shutdown must stop the pir-ext-actor thread"
    );
    let mut payload = json!({});
    assert!(
        runtime.emit("input", &mut payload).await.is_err(),
        "the actor mailbox must be closed after shutdown"
    );
}

// ---------------------------------------------------------------------------
// context / context_with_system (loop direct)
// ---------------------------------------------------------------------------

fn single_response_stream() -> EventStreamFn {
    let empty = AssistantMessage {
        api: "test".into(),
        provider: "test".into(),
        model: "hook-coverage-model".into(),
        content: Vec::new(),
        stop_reason: StopReason::Pending,
        ..AssistantMessage::default()
    };
    let done = Arc::new(AssistantMessage {
        api: "test".into(),
        provider: "test".into(),
        model: "hook-coverage-model".into(),
        content: vec![pir::ai::types::ContentBlock::Text(
            pir::ai::types::TextContent {
                text: "ok".into(),
                text_signature: None,
            },
        )],
        stop_reason: StopReason::Stop,
        ..AssistantMessage::default()
    });
    let events = vec![
        AssistantMessageEvent::Start {
            partial: Arc::new(empty.clone()),
        },
        AssistantMessageEventStream::done_event(TerminalReason::Stop, (*done).clone()),
    ];
    Box::new(move |_request| {
        let events = events.clone();
        Box::pin(async move {
            let stream = AssistantMessageEventStream::new();
            let sink = stream.clone();
            tokio::spawn(async move {
                for event in events {
                    sink.push(event);
                }
            });
            stream
        })
    })
}

fn context_loop_deps(cwd: &Path, handle: ExtensionHandle, host: Arc<PluginHost>) -> LoopDeps {
    LoopDeps {
        tools: Arc::new(ToolRegistry::new()),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        extensions: Some(handle),
        plugin_events: Some(host),
        max_turns: 2,
        cwd: cwd.to_path_buf(),
        ..LoopDeps::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn context_events_reach_native_plugins_in_a_builtin_only_plan() {
    let temp = TempDir::new("context-builtin");
    let handle = ExtensionHandle::start_plan_only(extension_config(&temp.0, false));
    let (recorder, events) = EventRecorder::new();
    let host = Arc::new(PluginHost::build(
        Some(handle.clone()),
        vec![recorder as Arc<dyn AgentPlugin>],
    ));

    let deps = context_loop_deps(&temp.0, handle, host);
    let options = LoopOptions {
        event_stream: Some(single_response_stream()),
        ..LoopOptions::default()
    };
    run_with_options(deps, Context::default(), None, options).await;

    assert_eq!(
        event_count(&events, "context"),
        1,
        "one provider request => one context event"
    );
    assert_eq!(event_count(&events, "context_with_system"), 1);
    let payload = &payloads(&events, "context")[0];
    assert_eq!(payload["type"], json!("context"));
    assert!(payload["messages"].is_array(), "{payload}");
}

#[tokio::test(flavor = "current_thread")]
async fn context_events_reach_both_faces_once_with_a_js_runtime() {
    let temp = TempDir::new("context-js");
    let extension_path = temp.0.join("context.ts");
    std::fs::write(
        &extension_path,
        r#"
        export default function (pi) {
            globalThis.__counts = { context: 0, context_with_system: 0 };
            pi.on("context", () => { globalThis.__counts.context += 1; });
            pi.on("context_with_system", () => { globalThis.__counts.context_with_system += 1; });
        }
        "#,
    )
    .expect("write extension");
    let Ok(runtime) = ExtensionHandle::start(extension_config(&temp.0, false)) else {
        return;
    };
    let loaded = runtime
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let (recorder, events) = EventRecorder::new();
    let host = Arc::new(PluginHost::build(
        Some(runtime.clone()),
        vec![recorder as Arc<dyn AgentPlugin>],
    ));
    let deps = context_loop_deps(&temp.0, runtime.clone(), host);
    run_with_options(
        deps,
        Context::default(),
        None,
        LoopOptions {
            event_stream: Some(single_response_stream()),
            ..LoopOptions::default()
        },
    )
    .await;

    assert_eq!(event_count(&events, "context"), 1);
    assert_eq!(event_count(&events, "context_with_system"), 1);
    let js: Value = runtime
        .eval_json("JSON.stringify(globalThis.__counts)")
        .await
        .expect("eval JS counts");
    assert_eq!(
        js,
        json!({ "context": 1, "context_with_system": 1 }),
        "each face observes the event exactly once"
    );
}

// ---------------------------------------------------------------------------
// Print host: agent_settled / provider_stream_event
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn print_agent_settled_reaches_native_plugins() {
    let (recorder, events) = EventRecorder::new();
    let host = Arc::new(PluginHost::build(
        None,
        vec![recorder as Arc<dyn AgentPlugin>],
    ));
    dispatch_print_agent_settled(Some(&host), None, true).await;
    assert_eq!(event_count(&events, "agent_settled"), 1);
    assert_eq!(
        payloads(&events, "agent_settled")[0]["aborted"],
        json!(true)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn print_agent_settled_reaches_both_faces_once() {
    let temp = TempDir::new("print-settled");
    let extension_path = temp.0.join("settled.ts");
    std::fs::write(
        &extension_path,
        r#"
        export default function (pi) {
            globalThis.__settled = [];
            pi.on("agent_settled", (event) => {
                globalThis.__settled.push(event.aborted);
            });
        }
        "#,
    )
    .expect("write extension");
    let Ok(runtime) = ExtensionHandle::start(extension_config(&temp.0, false)) else {
        return;
    };
    let loaded = runtime
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let (recorder, events) = EventRecorder::new();
    let host = Arc::new(PluginHost::build(
        Some(runtime.clone()),
        vec![recorder as Arc<dyn AgentPlugin>],
    ));
    dispatch_print_agent_settled(Some(&host), Some(&runtime), false).await;

    assert_eq!(event_count(&events, "agent_settled"), 1);
    let js: Value = runtime
        .eval_json("JSON.stringify(globalThis.__settled)")
        .await
        .expect("eval JS settled");
    assert_eq!(
        js,
        json!([false]),
        "the JS handler observes agent_settled exactly once"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn print_provider_stream_event_reaches_both_faces_once() {
    let temp = TempDir::new("print-stream");
    let extension_path = temp.0.join("stream.ts");
    std::fs::write(
        &extension_path,
        r#"
        export default function (pi) {
            globalThis.__streamEvents = 0;
            pi.on("provider_stream_event", () => {
                globalThis.__streamEvents = (globalThis.__streamEvents ?? 0) + 1;
            });
        }
        "#,
    )
    .expect("write extension");
    let Ok(runtime) = ExtensionHandle::start(extension_config(&temp.0, false)) else {
        return;
    };
    let loaded = runtime
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let (recorder, events) = EventRecorder::new();
    let host = Arc::new(PluginHost::build(
        Some(runtime.clone()),
        vec![recorder as Arc<dyn AgentPlugin>],
    ));
    let hooks = print_transport_hooks_with_model(Some(&host), Some(&runtime), Some(&fake_model()));
    let hook = hooks
        .on_provider_stream_event
        .expect("print installs the provider stream hook");
    hook(&json!({ "delta": "one" }));

    wait_for_count(&events, "provider_stream_event", 1).await;
    assert_eq!(
        payloads(&events, "provider_stream_event")[0]["type"],
        json!("provider_stream_event")
    );
    // The hook is fire-and-forget through the runtime's backlog; give the JS
    // adapter the same bounded window as the native side.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let js: Value = runtime
            .eval_json("JSON.stringify(globalThis.__streamEvents)")
            .await
            .expect("eval JS stream count");
        if js == json!(1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "JS provider_stream_event never arrived: {js}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// TUI capability: tools-expanded shares the JS/TUI cell
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn native_tools_expanded_reads_and_writes_the_shared_cell() {
    let fixture = NativeDialogFixture::new("tools-expanded");
    let ui = fixture.ui();

    assert!(!ui.tools_expanded(), "default is collapsed");
    ui.set_tools_expanded(true);
    assert!(
        fixture.handle.tools_expanded(),
        "native write reaches the shared cell"
    );
    // A TUI-side toggle (what Ctrl+O writes) is visible to the native read.
    fixture.handle.set_tools_expanded(false);
    assert!(!ui.tools_expanded());
}

// ---------------------------------------------------------------------------
// mcp_servers_change: native + JS delivery from the MCP registry rebuild
// ---------------------------------------------------------------------------

/// Isolate the global agent dir (where `mcp.json` lives) for one test and
/// restore the previous value on drop.
struct AgentDirGuard(Option<std::ffi::OsString>);

impl AgentDirGuard {
    fn isolate(path: &Path) -> Self {
        let previous = std::env::var_os("PIR_AGENT_DIR");
        std::env::set_var("PIR_AGENT_DIR", path);
        Self(previous)
    }
}

impl Drop for AgentDirGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
            None => std::env::remove_var("PIR_AGENT_DIR"),
        }
    }
}

/// One registry change (add, remove, reload rebuild) coalesces into one
/// upstream-shaped event; the JS face keeps its historic direct-handle copy
/// and the native plugin receives exactly one `emit_native` copy. Unrelated
/// dispatches emit nothing.
#[cfg(feature = "mcp")]
#[tokio::test(flavor = "current_thread")]
async fn mcp_servers_change_reaches_the_native_face_once_per_registry_change() {
    use pir::plugins::builtin::McpPlugin;

    let temp = TempDir::new("mcp-native-change");
    let _agent_dir = AgentDirGuard::isolate(&temp.0);
    let extension_path = temp.0.join("mcp_ext.ts");
    std::fs::write(
        &extension_path,
        r#"
        export default function (pi) {
            globalThis.__mcpChangeCount = 0;
            pi.on("mcp_servers_change", () => {
                globalThis.__mcpChangeCount = (globalThis.__mcpChangeCount ?? 0) + 1;
            });
            pi.registerMcpServer("native-face", {
                command: "definitely-not-a-real-command-13579",
                enabled: false,
            });
        }
        "#,
    )
    .expect("write extension");
    let Ok(handle) = ExtensionHandle::start(extension_config(&temp.0, false)) else {
        return; // build without the extensions feature
    };
    let loaded = handle
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let (recorder, events) = EventRecorder::new();
    let host = PluginHost::build(
        Some(handle.clone()),
        vec![
            Arc::new(McpPlugin::with_cwd(&temp.0, &temp.0)) as Arc<dyn AgentPlugin>,
            recorder as Arc<dyn AgentPlugin>,
        ],
    );

    // The registration is queued at host build; the first dispatch's refresh
    // tail flushes it to both faces exactly once.
    let mut payload = json!({ "type": "session_start", "reason": "startup" });
    host.emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert_eq!(
        event_count(&events, "mcp_servers_change"),
        1,
        "one native copy for the build-time registration"
    );
    let first = payloads(&events, "mcp_servers_change")[0].clone();
    assert_eq!(first["type"], json!("mcp_servers_change"));
    let servers = first["servers"].as_array().expect("servers array");
    assert_eq!(servers.len(), 1, "extension-registered only: {servers:?}");
    assert_eq!(servers[0]["name"], json!("native-face"));
    assert_eq!(
        servers[0]["config"]["command"],
        json!("definitely-not-a-real-command-13579")
    );
    let js: Value = handle
        .eval_json("JSON.stringify(globalThis.__mcpChangeCount)")
        .await
        .expect("eval JS change count");
    assert_eq!(js, json!(1), "the JS face keeps its single copy");

    // No per-dispatch re-emission: an event that changes nothing delivers
    // nothing.
    let mut payload = json!({ "type": "turn_start" });
    host.emit("turn_start", &mut payload)
        .await
        .expect("emit turn_start");
    assert_eq!(
        event_count(&events, "mcp_servers_change"),
        1,
        "an unrelated dispatch must not re-emit the change"
    );

    // Removing the extension-registered server (a later JS definition write
    // that the next rebuild observes) delivers again.
    let remove_path = temp.0.join("mcp_remove.ts");
    std::fs::write(
        &remove_path,
        r#"
        export default function (pi) {
            pi.unregisterMcpServer("native-face");
        }
        "#,
    )
    .expect("write remove fixture");
    let loaded = handle
        .load(std::slice::from_ref(&remove_path))
        .await
        .expect("load remove");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    let mut payload = json!({ "type": "turn_start" });
    host.emit("turn_start", &mut payload)
        .await
        .expect("emit turn_start");
    assert_eq!(
        event_count(&events, "mcp_servers_change"),
        2,
        "the removal rebuild delivers one native copy"
    );
    let second = payloads(&events, "mcp_servers_change")[1].clone();
    assert!(
        second["servers"].as_array().expect("servers").is_empty(),
        "the removed server is gone from the payload: {second}"
    );
    let js: Value = handle
        .eval_json("JSON.stringify(globalThis.__mcpChangeCount)")
        .await
        .expect("eval JS change count");
    assert_eq!(js, json!(2), "the JS face sees the removal once");

    // `PluginHost::reload` rebuilds and flushes too, with no dispatch in
    // between.
    let reload_path = temp.0.join("mcp_reload.ts");
    std::fs::write(
        &reload_path,
        r#"
        export default function (pi) {
            pi.registerMcpServer("reload-face", {
                command: "definitely-not-a-real-command-24680",
                enabled: false,
            });
        }
        "#,
    )
    .expect("write reload fixture");
    let loaded = handle
        .load(std::slice::from_ref(&reload_path))
        .await
        .expect("load reload");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    host.reload().await.expect("reload");
    assert_eq!(
        event_count(&events, "mcp_servers_change"),
        3,
        "the reload rebuild delivers one native copy"
    );
    let third = payloads(&events, "mcp_servers_change")[2].clone();
    assert!(
        third["servers"]
            .as_array()
            .expect("servers")
            .iter()
            .any(|server| server["name"] == json!("reload-face")),
        "the reload-registered server is in the payload: {third}"
    );
    host.shutdown().await;
}

// ---------------------------------------------------------------------------
// session_before_tree: cancellable pre-navigation, both faces
// ---------------------------------------------------------------------------

/// A native plugin that cancels every `session_before_tree` and records the
/// payloads it observed.
struct BeforeTreeCancel {
    info: PluginInfo,
    events: Recorded,
}

impl AgentPlugin for BeforeTreeCancel {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["session_before_tree".to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((event.name.to_string(), event.payload.clone()));
            if event.name.as_ref() == "session_before_tree" {
                PluginEventOutcome::SessionBefore { cancel: true }
            } else {
                PluginEventOutcome::None
            }
        })
    }
}

/// Counts summarizer requests so a cancelled navigation can prove the model
/// was never called.
struct CountingProvider {
    calls: Arc<AtomicUsize>,
}

impl pir::ai::provider::Provider for CountingProvider {
    fn id(&self) -> &str {
        "hook-fake"
    }

    fn name(&self) -> &str {
        "Hook coverage counting fake"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: pir::ai::types::StreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "test".into(),
                provider: "hook-fake".into(),
                model: "hook-coverage-model".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "extension summary".into(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                ..AssistantMessage::default()
            },
        ));
        stream
    }

    fn api_key_auth(&self) -> Vec<pir::ai::auth::ApiKeyAuth> {
        Vec::new()
    }
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn assistant_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "test".into(),
        provider: "hook-fake".into(),
        model: "hook-coverage-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        ..AssistantMessage::default()
    }
}

/// `u1 -> a1` trunk with a `u1 -> u2 -> u3` branch; the leaf is `u3`.
fn branched_session() -> (SessionManager, Vec<String>) {
    let mut session = SessionManager::in_memory(".", None).expect("in-memory session");
    let u1 = session
        .append_message(user_message("first"))
        .expect("append u1");
    let a1 = session
        .append_message(Message::Assistant(assistant_message("answer")))
        .expect("append a1");
    session.branch(&u1).expect("branch");
    let u2 = session
        .append_message(user_message("second"))
        .expect("append u2");
    let u3 = session
        .append_message(user_message("third"))
        .expect("append u3");
    (session, vec![u1, a1, u2, u3])
}

fn tree_agent_config(
    cwd: &Path,
    handle: ExtensionHandle,
    storage: SessionManager,
    calls: Arc<AtomicUsize>,
) -> AgentSessionConfig {
    let mut config = session_config(cwd, &handle);
    config.session = Some(storage);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(CountingProvider { calls }));
    config.provider_registry = Arc::new(registry);
    config
}

#[tokio::test(flavor = "current_thread")]
async fn session_before_tree_native_cancel_keeps_the_leaf_and_skips_summary() {
    let temp = TempDir::new("tree-native-cancel");
    let (storage, ids) = branched_session();
    let [u1, a1, _u2, u3] = [
        ids[0].clone(),
        ids[1].clone(),
        ids[2].clone(),
        ids[3].clone(),
    ];
    let handle = ExtensionHandle::start_plan_only(extension_config(&temp.0, false));
    let events: Recorded = Arc::new(Mutex::new(Vec::new()));
    let cancel = Arc::new(BeforeTreeCancel {
        info: PluginInfo::inline("tree-cancel"),
        events: Arc::clone(&events),
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let session = AgentSession::new_arc_with_plugins(
        tree_agent_config(&temp.0, handle, storage, Arc::clone(&calls)),
        vec![cancel],
    );

    let outcome = session
        .navigate_tree(&a1, true, None, false, 16_384)
        .await
        .expect("session storage");
    let navigation = outcome.expect("navigation");
    assert!(navigation.cancelled, "the native handler cancelled");
    assert!(!navigation.aborted);
    assert_eq!(navigation.summary_entry_id, None);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a cancelled navigation never calls the summarizer"
    );
    let storage = session.session_manager().expect("session storage");
    assert_eq!(
        storage.lock().await.leaf_id(),
        Some(u3.as_str()),
        "the leaf did not move"
    );

    let records = payloads(&events, "session_before_tree");
    assert_eq!(records.len(), 1, "exactly one native copy");
    let preparation = &records[0]["preparation"];
    assert_eq!(preparation["targetId"], json!(a1));
    assert_eq!(preparation["oldLeafId"], json!(u3));
    assert_eq!(preparation["commonAncestorId"], json!(u1));
    assert_eq!(preparation["userWantsSummary"], json!(true));
    assert_eq!(
        preparation["entriesToSummarize"].as_array().map(Vec::len),
        Some(2),
        "the abandoned u2 -> u3 branch: {preparation}"
    );
    assert_eq!(records[0]["signal"]["aborted"], json!(false));
}

#[tokio::test(flavor = "current_thread")]
async fn session_before_tree_js_cancel_keeps_the_leaf_and_is_delivered_once() {
    let temp = TempDir::new("tree-js-cancel");
    let extension_path = temp.0.join("tree_cancel.ts");
    std::fs::write(
        &extension_path,
        r#"
        export default function (pi) {
            globalThis.__treeEvents = 0;
            globalThis.__treeTarget = null;
            pi.on("session_before_tree", (event) => {
                globalThis.__treeEvents += 1;
                globalThis.__treeTarget = event.preparation.targetId;
                return { cancel: true };
            });
        }
        "#,
    )
    .expect("write extension");
    let Ok(handle) = ExtensionHandle::start(extension_config(&temp.0, false)) else {
        return;
    };
    let loaded = handle
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let (storage, ids) = branched_session();
    let [_u1, a1, _u2, u3] = [
        ids[0].clone(),
        ids[1].clone(),
        ids[2].clone(),
        ids[3].clone(),
    ];
    let (recorder, events) = EventRecorder::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let session = AgentSession::new_arc_with_plugins(
        tree_agent_config(&temp.0, handle.clone(), storage, Arc::clone(&calls)),
        vec![recorder],
    );

    let navigation = session
        .navigate_tree(&a1, true, None, false, 16_384)
        .await
        .expect("session storage")
        .expect("navigation");
    assert!(navigation.cancelled, "the JS handler cancelled");
    assert_eq!(navigation.summary_entry_id, None);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no summarizer request");
    let storage = session.session_manager().expect("session storage");
    assert_eq!(
        storage.lock().await.leaf_id(),
        Some(u3.as_str()),
        "the leaf did not move"
    );

    // The JS adapter sits before inline plugins in this test's plan, so its
    // `cancel: true` short-circuits the ordered list (upstream runner
    // semantics: the first cancel ends the emit). The non-cancel test below
    // pins the "one copy per face" invariant.
    let js: Value = handle
        .eval_json("JSON.stringify(globalThis.__treeEvents)")
        .await
        .expect("eval JS tree count");
    assert_eq!(js, json!(1), "the JS face sees the event exactly once");
    let js_target: Value = handle
        .eval_json("JSON.stringify(globalThis.__treeTarget)")
        .await
        .expect("eval JS tree target");
    assert_eq!(js_target, json!(a1), "the JS face got the preparation");
    assert_eq!(
        event_count(&events, "session_tree"),
        0,
        "a cancelled navigation emits no session_tree"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_before_tree_non_cancel_handler_leaves_navigation_unchanged() {
    let temp = TempDir::new("tree-js-pass");
    let extension_path = temp.0.join("tree_pass.ts");
    std::fs::write(
        &extension_path,
        r#"
        export default function (pi) {
            globalThis.__treeEvents = 0;
            pi.on("session_before_tree", () => {
                globalThis.__treeEvents += 1;
            });
        }
        "#,
    )
    .expect("write extension");
    let Ok(handle) = ExtensionHandle::start(extension_config(&temp.0, false)) else {
        return;
    };
    let loaded = handle
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let (storage, ids) = branched_session();
    let a1 = ids[1].clone();
    let (recorder, events) = EventRecorder::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let session = AgentSession::new_arc_with_plugins(
        tree_agent_config(&temp.0, handle.clone(), storage, Arc::clone(&calls)),
        vec![recorder],
    );

    let navigation = session
        .navigate_tree(&a1, false, None, false, 16_384)
        .await
        .expect("session storage")
        .expect("navigation");
    assert!(!navigation.cancelled, "a non-cancel handler passes");
    assert!(!navigation.aborted);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no summary requested");
    let storage = session.session_manager().expect("session storage");
    assert_eq!(
        storage.lock().await.leaf_id(),
        Some(a1.as_str()),
        "the leaf moved to the target"
    );

    assert_eq!(
        event_count(&events, "session_before_tree"),
        1,
        "one native copy"
    );
    let js: Value = handle
        .eval_json("JSON.stringify(globalThis.__treeEvents)")
        .await
        .expect("eval JS tree count");
    assert_eq!(js, json!(1), "one JS copy");
}

// ---------------------------------------------------------------------------
// terminal_input (raw terminal input, TUI producer)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn terminal_input_reaches_the_native_fan_out_exactly_once() {
    let temp = TempDir::new("terminal-input");
    // Plan-only handle: the JS adapter is present in the ordered list but has
    // no runtime; the native recorder is still reached exactly once.
    let handle = ExtensionHandle::start_plan_only(extension_config(&temp.0, true));
    let (recorder, events) = EventRecorder::new();
    let host = PluginHost::build(Some(handle), vec![recorder as Arc<dyn AgentPlugin>]);
    assert!(
        host.has_terminal_input_handlers(),
        "the recorder's EventInterest::All registers terminal-input interest"
    );

    let action = host.dispatch_terminal_input("!").await;
    assert_eq!(
        action,
        TerminalInputAction::Pass,
        "a pass handler changes nothing"
    );

    let rows = events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let terminal: Vec<_> = rows
        .iter()
        .filter(|(event, _)| event == "terminal_input")
        .collect();
    assert_eq!(terminal.len(), 1, "one native copy per physical input");
    assert_eq!(
        terminal[0].1,
        json!({ "data": "!" }),
        "the native payload is the canonical raw chunk"
    );
}
