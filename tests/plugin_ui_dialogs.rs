//! Native `UiService` dialogs on the shared JS dialog pipeline.
//!
//! `ctx.ui.select/confirm/input/editor` in a Rust plugin must ride the SAME
//! request/reply channel a JS extension uses:
//!
//! - the request is one ordinary
//!   [`Notification::DialogRequest`](pir::extensions::Notification::DialogRequest)
//!   on the runtime's presentation queue (the seam these tests observe with
//!   `ExtensionHandle::take_notifications`, exactly like `ctx_ui_parity.rs`),
//!   with the same `kind`/`title`/`options` wire fields;
//! - the reply travels back through the host's
//!   [`NativeDialogCell`] with the same result mapping the JS prelude applies
//!   (`select`/`input`/`editor` → string, `confirm` → bool, `null`/Esc →
//!   `None`);
//! - the renderer re-entrancy guard suppresses a request made while a render
//!   callback owns the UI thread (the JS `uiDialog` host fn's `has_ui` rule).
//!
//! Everything is in-memory except the session file under `std::env::temp_dir`.
//! A plan-only extension handle supplies the shared presentation queue without
//! constructing QuickJS.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::{json, Value as JsonValue};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{Context, Model, ModelCost, ModelCostRates, StreamOptions, ThinkingLevel};
use pir::extensions::{ExtensionConfig, ExtensionHandle, Notification};
use pir::plugins::host::{NativeDialogCell, NATIVE_DIALOG_ID_BASE};
use pir::plugins::plugin::services::{DialogOptions, PluginServiceError, SelectOption, UiService};
use pir::plugins::plugin::{AgentPlugin, PluginContext};
use pir::plugins::PluginInfo;
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Captures the context bound to the session (the native service handle).
struct ProbePlugin {
    info: PluginInfo,
    contexts: Mutex<Vec<PluginContext>>,
}

impl ProbePlugin {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline("native-dialog-probe"),
            contexts: Mutex::new(Vec::new()),
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
}

impl AgentPlugin for ProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn on_context(&self, ctx: &PluginContext) {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.clone());
    }
}

struct FakeProvider;

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "dialog-fake"
    }

    fn name(&self) -> &str {
        "Dialog fake provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        AssistantMessageEventStream::new()
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
}

fn fake_model() -> Model {
    Model {
        id: "dialog-model".to_owned(),
        name: "Dialog Model".to_owned(),
        api: "openai-completions".to_owned(),
        provider: "dialog-fake".to_owned(),
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

fn session_config(cwd: &Path, handle: &ExtensionHandle) -> AgentSessionConfig {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider));
    let session_dir = cwd.join("sessions");
    AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::High,
        tools: ToolRegistry::new(),
        session: Some(
            SessionManager::create(cwd.to_str().expect("utf-8 temp path"), &session_dir)
                .expect("create session"),
        ),
        compaction: None,
        max_turns: 4,
        system_prompt: Some("native dialog test prompt".to_owned()),
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: Some(handle.clone()),
        tool_selection: None,
    }
}

/// Plan-only handle: the shared presentation queue, the reply/probe accessors
/// and the renderer guard, without constructing QuickJS.
fn dialog_handle(cwd: &Path, has_ui: bool) -> ExtensionHandle {
    ExtensionHandle::start_plan_only(ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "interactive".to_owned(),
        has_ui,
        model: Some("dialog-model".to_owned()),
        session_id: Some("native-dialog-test".to_owned()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-plugin-ui-dialogs-{label}-{}-{}",
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

/// One live session plus the shared queue handle and the host's reply cell.
struct Fixture {
    _temp: TempDir,
    session: Arc<AgentSession>,
    handle: ExtensionHandle,
    plugin: Arc<ProbePlugin>,
}

impl Fixture {
    fn new(label: &str, has_ui: bool) -> Self {
        let temp = TempDir::new(label);
        let handle = dialog_handle(temp.path(), has_ui);
        let plugin = ProbePlugin::new();
        let session = AgentSession::new_arc_with_plugins(
            session_config(temp.path(), &handle),
            vec![plugin.clone() as Arc<dyn AgentPlugin>],
        );
        Self {
            _temp: temp,
            session,
            handle,
            plugin,
        }
    }

    fn ui(&self) -> Arc<dyn UiService> {
        self.plugin
            .last_context()
            .ui_arc()
            .expect("ui service is bound with a presentation sink")
    }

    fn cell(&self) -> Arc<NativeDialogCell> {
        self.session
            .plugin_host()
            .expect("the session has a plugin host")
            .native_dialogs()
    }

    /// Drain the pending `DialogRequest`s in queue (arrival) order — the same
    /// observation point the JS dialog tests use.
    fn requests(&self) -> Vec<DialogRequest> {
        self.handle
            .take_notifications(|notification| {
                matches!(notification, Notification::DialogRequest { .. })
            })
            .into_iter()
            .filter_map(|notification| match notification {
                Notification::DialogRequest {
                    id,
                    kind,
                    title,
                    options,
                } => Some(DialogRequest {
                    id,
                    kind,
                    title,
                    options,
                }),
                _ => None,
            })
            .collect()
    }
}

struct DialogRequest {
    id: u64,
    kind: String,
    title: Option<String>,
    options: JsonValue,
}

// ---------------------------------------------------------------------------
// Request wire shape / reply mapping
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn native_select_is_visible_on_the_shared_dialog_queue_and_resolves() {
    let fixture = Fixture::new("select", true);
    let ui = fixture.ui();

    let pending = ui.select(
        "Pick one",
        &[
            SelectOption::new("First", "a"),
            SelectOption::new("Second", "b"),
        ],
        DialogOptions::default(),
    );

    // ONE entry on the SAME queue the JS host fn writes; no mirrored
    // notification and no second queue.
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1, "one request per call");
    let request = &requests[0];
    assert_eq!(request.kind, "select");
    assert_eq!(request.title.as_deref(), Some("Pick one"));
    assert_eq!(request.options["title"], "Pick one");
    assert_eq!(
        request.options["options"],
        json!([
            { "value": "a", "label": "First" },
            { "value": "b", "label": "Second" },
        ]),
        "select choices use the prelude's {{value, label}} wire shape"
    );
    assert!(
        NativeDialogCell::is_native_dialog_id(request.id),
        "native ids use the reserved top range"
    );
    assert!(request.id >= NATIVE_DIALOG_ID_BASE);

    // The TUI responder's native branch resolves the future with the chosen
    // option.
    assert!(fixture.cell().respond(request.id, json!("b")));
    assert_eq!(
        pending.await.expect("the select future resolves"),
        Some("b".to_owned())
    );
    assert_eq!(fixture.cell().pending(), 0, "a settled waiter is removed");
}

#[tokio::test(flavor = "current_thread")]
async fn native_cancel_is_none_and_confirm_false_stays_false() {
    let fixture = Fixture::new("cancel-confirm", true);
    let ui = fixture.ui();

    // Esc/cancel arrives as `null` from the overlay's cancel path.
    let pending = ui.select(
        "Pick one",
        &[SelectOption::text("a")],
        DialogOptions::default(),
    );
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert!(fixture.cell().respond(requests[0].id, JsonValue::Null));
    assert_eq!(
        pending.await.expect("a cancelled select resolves"),
        None,
        "cancel normalizes to None"
    );

    // A false confirm is a real answer, never a cancel.
    let pending = ui.confirm("Proceed?", "sure?", DialogOptions::default());
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.kind, "confirm");
    assert_eq!(request.title.as_deref(), Some("Proceed?"));
    assert_eq!(request.options["message"], "sure?");
    assert!(fixture.cell().respond(request.id, json!(false)));
    assert_eq!(pending.await.expect("the confirm resolves"), Some(false));
}

#[tokio::test(flavor = "current_thread")]
async fn native_input_and_editor_return_the_submitted_text() {
    let fixture = Fixture::new("input-editor", true);
    let ui = fixture.ui();

    let pending = ui.input("Your name", Some("type here"), DialogOptions::default());
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.kind, "input");
    assert_eq!(request.options["placeholder"], "type here");
    assert!(fixture.cell().respond(request.id, json!("Ada")));
    assert_eq!(
        pending.await.expect("the input future resolves"),
        Some("Ada".to_owned())
    );

    // An absent placeholder is omitted, matching `JSON.stringify`'s
    // `undefined` handling in the JS prelude.
    let pending = ui.editor("Compose", Some("draft"), DialogOptions::default());
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.kind, "editor");
    assert_eq!(request.options["prefill"], "draft");
    assert!(fixture
        .cell()
        .respond(request.id, json!("line one\nline two")));
    assert_eq!(
        pending.await.expect("the editor future resolves"),
        Some("line one\nline two".to_owned())
    );

    let pending = ui.input("Your name", None, DialogOptions::default());
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].options.get("placeholder").is_none(),
        "no placeholder => no key on the wire"
    );
    assert!(fixture.cell().respond(requests[0].id, JsonValue::Null));
    assert_eq!(pending.await.expect("cancel resolves"), None);
}

// ---------------------------------------------------------------------------
// Timeout / cancellation / concurrency
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn native_dialog_timeout_is_a_service_error_and_leaves_no_waiter() {
    let fixture = Fixture::new("timeout", true);
    let ui = fixture.ui();

    let pending = ui.input(
        "Your name",
        None,
        DialogOptions::with_timeout(Duration::from_millis(80)),
    );
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1, "the request announced itself");
    assert_eq!(
        requests[0].options["timeout"], 80,
        "the deadline rides the same `timeout` wire key the JS opts use"
    );

    let error = pending
        .await
        .expect_err("an unanswered native dialog times out");
    match error {
        PluginServiceError::Failed(message) => {
            assert!(
                message.contains("timed out"),
                "the timeout error names the outcome: {message}"
            );
        }
        other => panic!("expected a failed capability, got {other:?}"),
    }
    assert!(
        fixture.requests().is_empty(),
        "a timed-out request is withdrawn from the shared queue"
    );
    assert_eq!(
        fixture.cell().pending(),
        0,
        "a timed-out waiter never leaks"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_native_dialog_withdraws_its_request_and_waiter() {
    let fixture = Fixture::new("dropped", true);
    let ui = fixture.ui();

    {
        let pending = ui.select(
            "Abandoned",
            &[SelectOption::text("a")],
            DialogOptions::default(),
        );
        drop(pending);
    }
    assert!(
        fixture.requests().is_empty(),
        "a dropped future removes its still-queued request"
    );
    assert_eq!(fixture.cell().pending(), 0);

    // Once the request was drained, dropping the future still retires the
    // waiter; a late reply is a harmless no-op.
    let pending = ui.select(
        "Drained",
        &[SelectOption::text("a")],
        DialogOptions::default(),
    );
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    let id = requests[0].id;
    drop(pending);
    assert_eq!(fixture.cell().pending(), 0);
    assert!(
        !fixture.cell().respond(id, json!("a")),
        "no waiter is revived by a late reply"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_native_dialogs_queue_in_arrival_order_and_resolve_independently() {
    let fixture = Fixture::new("concurrent", true);
    let ui = fixture.ui();

    let first = ui.select(
        "First",
        &[SelectOption::text("a")],
        DialogOptions::default(),
    );
    let second = ui.confirm("Second", "ok?", DialogOptions::default());

    let requests = fixture.requests();
    assert_eq!(requests.len(), 2, "both requests share the one queue");
    assert_eq!(requests[0].kind, "select");
    assert_eq!(requests[1].kind, "confirm");
    assert_ne!(
        requests[0].id, requests[1].id,
        "concurrent native dialogs get unique ids"
    );
    assert!(
        requests[0].id < requests[1].id,
        "ids are monotonic in arrival order"
    );

    // The TUI promotes queued overlays FIFO; each reply wakes exactly its
    // own waiter.
    assert!(fixture.cell().respond(requests[0].id, json!("a")));
    assert_eq!(
        first.await.expect("the first dialog resolves"),
        Some("a".to_owned())
    );
    assert!(fixture.cell().respond(requests[1].id, json!(true)));
    assert_eq!(
        second.await.expect("the second dialog resolves"),
        Some(true)
    );
    assert_eq!(fixture.cell().pending(), 0);
}

// ---------------------------------------------------------------------------
// Renderer re-entrancy guard / headless fail-closed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn renderer_ui_guard_suppresses_native_dialogs() {
    let fixture = Fixture::new("guard", true);
    let ui = fixture.ui();

    // The guard is the SAME per-runtime switch `invoke_renderer_sync` holds
    // for JS callbacks: it clears the runtime's `has_ui`, so a dialog asked
    // for inside the render window resolves immediately instead of parking.
    let suppressed = fixture.handle.with_renderer_ui_guard(|| {
        let future = ui.select(
            "Blocked",
            &[SelectOption::text("a")],
            DialogOptions::default(),
        );
        future.now_or_never()
    });
    assert_eq!(
        suppressed,
        Some(Ok(None)),
        "a dialog requested during a render resolves None immediately"
    );
    assert!(
        fixture.requests().is_empty(),
        "the suppressed request never reaches the queue"
    );

    // The guard restores the runtime posture on drop: the next call is live
    // again.
    let pending = ui.select(
        "After render",
        &[SelectOption::text("a")],
        DialogOptions::default(),
    );
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert!(fixture.cell().respond(requests[0].id, json!("a")));
    assert_eq!(
        pending.await.expect("the post-guard dialog resolves"),
        Some("a".to_owned())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn headless_native_dialogs_fail_closed_with_the_capability_error() {
    let fixture = Fixture::new("headless", false);
    let ui = fixture.ui();

    let error = ui
        .select("Pick", &[SelectOption::text("a")], DialogOptions::default())
        .await
        .expect_err("headless select fails closed");
    assert_eq!(error, PluginServiceError::capability("ui", "select"));

    let error = ui
        .confirm("Proceed?", "sure?", DialogOptions::default())
        .await
        .expect_err("headless confirm fails closed");
    assert_eq!(error, PluginServiceError::capability("ui", "confirm"));

    let error = ui
        .input("Name", None, DialogOptions::default())
        .await
        .expect_err("headless input fails closed");
    assert_eq!(error, PluginServiceError::capability("ui", "input"));

    let error = ui
        .editor("Edit", None, DialogOptions::default())
        .await
        .expect_err("headless editor fails closed");
    assert_eq!(error, PluginServiceError::capability("ui", "editor"));

    assert!(
        fixture.requests().is_empty(),
        "a headless session never enqueues a dialog"
    );
    assert_eq!(fixture.cell().pending(), 0);
}
