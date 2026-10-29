//! Live [`SessionService`] mutations and the host-retained session snapshot.
//!
//! Pins the two seams added on top of the read-only live binding:
//!
//! 1. `PluginHost::update_session_context` retains the pushed
//!    `SessionContextSnapshot` as one `Arc` (plus its `entries` field as a
//!    second `Arc`). `SessionService::snapshot`/`entries` return those exact
//!    `Arc`s through the service's weak host, so repeated reads are
//!    `Arc::ptr_eq` and a new push replaces them; an `Arc` taken before the
//!    push keeps observing the old payload.
//! 2. Mutating methods with a real backing are live: `set_active_tools` writes
//!    the session override cell synchronously, `set_model` /
//!    `set_thinking_level` schedule the existing async session method, `abort`
//!    cancels the in-flight run and `wait_for_idle` tracks the run claim. The
//!    methods that need UI/session replacement (`new_session`, `fork`,
//!    `navigate_tree`, `switch_session`, `reload`) keep the documented trait
//!    defaults, and scheduling without a tokio runtime is a no-op.
//!
//! Everything is in-memory except the session file under `std::env::temp_dir`.

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
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::plugins::plugin::services::{ForkOptions, NavigateTreeOptions, NewSessionOptions};
use pir::plugins::plugin::PluginContext;
use pir::plugins::{AgentPlugin, PluginInfo};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A native probe that records the context it is handed in `on_context`.
struct ProbePlugin {
    info: PluginInfo,
    contexts: Mutex<Vec<PluginContext>>,
    context_calls: AtomicUsize,
}

impl ProbePlugin {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            contexts: Mutex::new(Vec::new()),
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
}

impl AgentPlugin for ProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn on_context(&self, ctx: &PluginContext) {
        self.context_calls.fetch_add(1, Ordering::SeqCst);
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.clone());
    }
}

/// One core tool, so `setActiveTools` has a real name to activate.
struct ProbeTool;

impl AgentTool for ProbeTool {
    fn name(&self) -> &'static str {
        "probe-tool"
    }

    fn description(&self) -> &'static str {
        "probe tool for the live SessionService test"
    }

    fn parameters(&self) -> Option<JsonValue> {
        Some(json!({ "type": "object", "properties": {} }))
    }

    fn run(&self, _args: &JsonValue, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamMode {
    /// Answer immediately with a successful empty assistant message.
    Immediate,
    /// Stay pending until the request's abort signal fires, then settle as
    /// aborted — the fixture for a run that is genuinely in flight.
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
        vec![fake_model("model-a"), fake_model("model-b")]
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
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
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
                stream
            }
        }
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
}

fn fake_model(id: &str) -> Model {
    Model {
        id: id.to_owned(),
        name: format!("Model {id}"),
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
            "pir-plugin-session-{label}-{}-{}",
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
        model: fake_model("model-a"),
        api_key: None,
        thinking: ThinkingLevel::High,
        tools,
        session: Some(SessionManager::create(&cwd, &session_dir).expect("create session")),
        compaction: None,
        max_turns: 4,
        system_prompt: Some("plugin session actions test prompt".to_owned()),
        cwd: Some(temp.path().to_path_buf()),
        event_stream: None,
        extensions,
        tool_selection: None,
    }
}

/// A plan-only extension handle: the session takes the full JS-facing
/// snapshot-push path (so `entries` is built) without constructing QuickJS.
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

async fn wait_until(label: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// (1) Arc-retained snapshot / entries
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn pushed_snapshot_and_entries_are_arc_identical_until_the_next_push() {
    let temp = TempDir::new("snapshot");
    let plugin = ProbePlugin::new("snapshot-probe");
    let session = AgentSession::new_arc_with_plugins(
        session_config(
            &temp,
            StreamMode::Immediate,
            ToolRegistry::new(),
            Some(plan_only_handle(temp.path())),
        ),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    assert_eq!(plugin.context_count(), 1, "one context handoff per binding");
    let ctx = plugin.last_context();
    let service = ctx.session().expect("session service");

    // First push: the host retains the snapshot and its entries as Arcs.
    assert!(
        session.push_session_context_for_test().await,
        "the first push builds the session-derived payload"
    );
    let snapshot = service.snapshot().expect("host-retained snapshot");
    let entries = service.entries().expect("host-retained entries");
    assert!(
        Arc::ptr_eq(
            &snapshot,
            &service.snapshot().expect("second snapshot read")
        ),
        "a repeated snapshot read returns the same Arc"
    );
    assert!(
        Arc::ptr_eq(&entries, &service.entries().expect("second entries read")),
        "a repeated entries read returns the same Arc"
    );
    // The entries read is the snapshot's own field, retained separately.
    assert_eq!(*entries, snapshot.entries);
    let first_entries = (*entries).clone();

    // A prompt appends session entries; its post-run push replaces the cells.
    let terminal = session
        .prompt("next", Arc::new(|_| {}))
        .await
        .expect("prompt settles");
    assert_eq!(terminal.stop_reason, StopReason::Stop);

    let new_snapshot = service.snapshot().expect("new snapshot");
    let new_entries = service.entries().expect("new entries");
    assert!(
        !Arc::ptr_eq(&snapshot, &new_snapshot),
        "a new push replaces the snapshot Arc"
    );
    assert!(
        !Arc::ptr_eq(&entries, &new_entries),
        "a new push replaces the entries Arc"
    );
    assert_ne!(*new_entries, first_entries, "the run appended entries");
    // The old Arc is an immutable view of the pre-push payload.
    assert_eq!(*entries, first_entries);
    assert_eq!(snapshot.entries, first_entries);
}

// ---------------------------------------------------------------------------
// (2) Live mutators
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn live_mutators_reach_the_session_and_wait_for_idle_tracks_the_claim() {
    let temp = TempDir::new("actions");
    let plugin = ProbePlugin::new("actions-probe");
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ProbeTool));
    let session = AgentSession::new_arc_with_plugins(
        session_config(&temp, StreamMode::Hanging, tools, None),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    let ctx = plugin.last_context();
    let service = ctx.session().expect("session service");

    // `set_active_tools` is synchronous: it writes the live override cell and
    // the typed read observes it.
    service.set_active_tools(&[Arc::from("probe-tool")]);
    let active: Vec<String> = service
        .active_tools()
        .iter()
        .map(|name| name.to_string())
        .collect();
    assert_eq!(
        active,
        vec!["probe-tool".to_owned()],
        "the active-tool override is live"
    );

    // `set_model` is scheduled on the current runtime; poll the typed read.
    assert!(
        service.set_model(&fake_model("model-b")),
        "a live session plus runtime schedules the model switch"
    );
    wait_until("model switch", || {
        service.model().is_some_and(|model| model.id == "model-b")
    })
    .await;

    // `set_thinking_level` takes the same scheduled path.
    service.set_thinking_level(ThinkingLevel::Low);
    wait_until("thinking-level switch", || {
        service.thinking_level() == ThinkingLevel::Low
    })
    .await;

    // A hanging run owns the claim until `abort` fires the run signal.
    let run = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.prompt("hang", Arc::new(|_| {})).await })
    };
    wait_until("run to start", || session.is_streaming()).await;
    assert!(
        !service.is_idle(),
        "is_idle is false while a run owns the claim"
    );

    service.abort();
    tokio::time::timeout(Duration::from_secs(5), service.wait_for_idle())
        .await
        .expect("wait_for_idle resolves once the aborted run settles");
    let terminal = run
        .await
        .expect("prompt task joins")
        .expect("prompt result");
    assert_eq!(terminal.stop_reason, StopReason::Aborted);
    assert!(service.is_idle(), "the aborted run released the claim");
}

// ---------------------------------------------------------------------------
// (3) Unscheduled / unsupported methods stay honest no-ops
// ---------------------------------------------------------------------------

#[test]
fn unsupported_and_runtime_less_actions_are_honest_no_ops() {
    let temp = TempDir::new("defaults");
    let plugin = ProbePlugin::new("defaults-probe");
    // Keep the session alive: the bound context only holds a `Weak` to it.
    let _session = AgentSession::new_arc_with_plugins(
        session_config(&temp, StreamMode::Immediate, ToolRegistry::new(), None),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    let ctx = plugin.last_context();
    let service = ctx.session().expect("session service");
    let before_id = service.session_id().expect("session id");
    let before_model = service.model().expect("model").id.clone();

    // No tokio runtime is current in a plain `#[test]`: nothing is scheduled,
    // nothing panics, and the session cells are untouched.
    assert!(
        !service.set_model(&fake_model("model-b")),
        "no runtime means the switch cannot be scheduled"
    );
    service.set_thinking_level(ThinkingLevel::Low);
    service.set_session_name(Some("ignored"));
    service.set_label("missing-entry", Some("label"));
    service.compact(None);
    service.shutdown();
    service.abort();
    assert_eq!(
        service.model().expect("model").id,
        before_model,
        "an unscheduled model switch leaves the session model unchanged"
    );

    // These need UI/session replacement (or a return value the trait cannot
    // express) and keep the trait defaults: immediate, side-effect-free.
    service.new_session(NewSessionOptions::default());
    service.fork(None, ForkOptions::default());
    service.navigate_tree("missing-entry", NavigateTreeOptions::default());
    service.switch_session(Path::new("missing-session.jsonl"));
    service.reload();

    assert_eq!(
        service.session_id().as_deref(),
        Some(before_id.as_ref()),
        "the session is still the same one"
    );
    assert_eq!(service.model().expect("model").id, before_model);
    assert!(service.is_idle());
}
