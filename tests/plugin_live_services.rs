//! Live native plugin-service binding tests.
//!
//! These pin the production binding created by `AgentSession` construction:
//!
//! (a) a native plugin attached to a real SDK-built session receives
//!     `on_context` exactly once with non-`None` provider/session/status
//!     services,
//! (b) typed session reads match the live session (cwd, session id/path,
//!     model id/name, thinking level) with no JSON hop,
//! (c) a status set through the bound context is visible through
//!     `PluginHost::statuses()` (via `AgentSession::plugin_statuses`),
//! (d) the event bus delivers FIFO and drops nothing under a bounded burst,
//! (e) rebinding bumps the epoch and the old context reports stale.
//!
//! Everything is in-memory: a scripted provider that is never streamed, temp
//! session dirs, and no extension runtime.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value as JsonValue};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{Context, Model, ModelCost, ModelCostRates, StreamOptions, ThinkingLevel};
use pir::extensions::FlagValue;
use pir::plugins::plugin::context::PluginServiceError;
use pir::plugins::plugin::services::{ExtensionMode, PluginServices};
use pir::plugins::plugin::PluginContext;
use pir::plugins::{AgentPlugin, PluginHost, PluginInfo};
use pir::sdk::AgentBuilder;
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A native probe plugin: records every context the host hands it and
/// registers one boolean CLI flag (so the live `FlagService` has a real
/// definition to resolve).
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

    fn contexts(&self) -> Vec<PluginContext> {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn context_count(&self) -> usize {
        self.context_calls.load(Ordering::SeqCst)
    }

    fn last_context(&self) -> PluginContext {
        self.contexts()
            .last()
            .cloned()
            .expect("the session bound a context")
    }
}

impl AgentPlugin for ProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut pir::plugins::RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_flag(
            "probe-live-flag",
            "boolean",
            "Probe flag for the live FlagService",
            Some(FlagValue::Bool(true)),
        );
        Ok(())
    }

    fn on_context(&self, ctx: &PluginContext) {
        self.context_calls.fetch_add(1, Ordering::SeqCst);
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.clone());
    }
}

/// In-memory provider; its model list backs the session's typed model reads.
struct FakeProvider;

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
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        // The tests never prompt.
        AssistantMessageEventStream::new()
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

fn fake_registry() -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider));
    Arc::new(registry)
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-plugin-live-{label}-{}-{}",
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

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn session_config(temp: &TempDir) -> AgentSessionConfig {
    let cwd = temp.path().to_str().expect("utf-8 temp path").to_owned();
    let session_dir = temp.path().join("sessions");
    AgentSessionConfig {
        provider_registry: fake_registry(),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::High,
        tools: ToolRegistry::new(),
        session: Some(SessionManager::create(&cwd, &session_dir).expect("create session")),
        compaction: None,
        max_turns: 4,
        system_prompt: Some("live binding test prompt".to_owned()),
        cwd: Some(temp.path().to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

fn live_session(
    temp: &TempDir,
    plugin: &Arc<ProbePlugin>,
) -> (Arc<AgentSession>, Arc<ProbePlugin>) {
    let session = AgentSession::new_arc_with_plugins(
        session_config(temp),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    (session, Arc::clone(plugin))
}

// ---------------------------------------------------------------------------
// (a) exactly one context on a real SDK-built session
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn sdk_built_session_binds_context_exactly_once() {
    let temp = TempDir::new("sdk-context");
    let plugin = ProbePlugin::new("live-sdk-probe");
    let agent = AgentBuilder::new()
        .with_registry(fake_registry(), fake_model())
        .thinking_level(ThinkingLevel::High)
        .with_plugin(plugin.clone())
        .cwd(temp.path())
        .no_session()
        .build()
        .await
        .expect("build SDK agent");

    assert_eq!(
        plugin.context_count(),
        1,
        "on_context must fire exactly once per construction"
    );
    let contexts = plugin.contexts();
    assert_eq!(contexts.len(), 1);
    let ctx = &contexts[0];

    // The three services the task requires on every live binding.
    assert!(ctx.providers().is_some(), "provider service bound");
    assert!(ctx.session().is_some(), "session service bound");
    assert!(ctx.status().is_some(), "status service bound");
    // Additive services this lane also binds.
    assert!(ctx.events().is_some(), "event bus bound");
    assert!(ctx.flags().is_some(), "flag service bound");

    // Typed model/thinking straight off the live session.
    let session = ctx.session().expect("session service");
    let model = session.model().expect("live model");
    assert_eq!(model.id, "model-a");
    assert_eq!(model.name, "Model A");
    assert_eq!(session.thinking_level(), ThinkingLevel::High);

    // The SDK plugin identity is stamped on the handoff.
    assert_eq!(ctx.identity().name(), "live-sdk-probe");

    drop(agent);
}

// ---------------------------------------------------------------------------
// (b) typed reads match the real session, no JSON involved
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn typed_session_reads_match_the_live_session() {
    let temp = TempDir::new("typed-reads");
    let plugin = ProbePlugin::new("live-read-probe");
    let (session, plugin) = live_session(&temp, &plugin);

    let ctx = plugin.last_context();
    let service = ctx.session().expect("session service");

    // cwd: the same shared Path, compared typed.
    assert_eq!(
        service.cwd().expect("cwd").as_ref(),
        temp.path(),
        "cwd must be the live session cwd"
    );

    // Session identity: compared against the session's own typed cells.
    assert_eq!(
        service.session_id().as_deref(),
        session.session_id().as_deref(),
        "session id must be live"
    );
    assert_eq!(
        service.session_path().as_deref(),
        session.session_path().as_deref(),
        "session path must be live"
    );

    // Model and thinking level: typed values, no string/JSON hop.
    let model: Arc<Model> = service.model().expect("live model");
    assert_eq!(model.id, session.model().id);
    assert_eq!(model.name, session.model().name);
    let level: ThinkingLevel = service.thinking_level();
    assert_eq!(level, session.thinking_level());

    // Mode/idle/trust reads.
    assert_eq!(service.mode(), ExtensionMode::Headless);
    assert!(service.is_idle(), "no run is in flight");
    assert!(!service.has_pending_messages());

    // Scoped models and tool/command surfaces are typed slices.
    let scoped: Arc<[pir::ai::providers::resolution::ScopedModel]> = service.scoped_models();
    assert!(scoped.is_empty());
    let _tools = service.all_tools();
    let _active = service.active_tools();
    let _commands = service.commands();

    // ProviderService physical-model resolution is typed too.
    let physical = ctx
        .providers()
        .expect("provider service")
        .physical_model("fake", "model-a")
        .expect("physical model");
    assert_eq!(physical.id, "model-a");
    assert_eq!(physical.provider, "fake");

    // FlagService resolves the native registration's typed default.
    assert_eq!(
        ctx.flags().expect("flag service").value("probe-live-flag"),
        Some(FlagValue::Bool(true))
    );
    assert_eq!(
        ctx.flags().expect("flag service").value("not-registered"),
        None
    );
}

// ---------------------------------------------------------------------------
// (c) status cells reach PluginHost::statuses()
// ---------------------------------------------------------------------------

#[test]
fn status_set_through_context_is_visible_through_host_statuses() {
    let temp = TempDir::new("status");
    let plugin = ProbePlugin::new("live-status-probe");
    let (session, plugin) = live_session(&temp, &plugin);
    let ctx = plugin.last_context();

    let status = ctx.require_status().expect("status service");
    status.set_status("build", Some("running"));
    status.set_status("build", Some("passing"));
    status.set_status("queue", Some("2"));

    let statuses = session.plugin_statuses();
    assert!(
        statuses
            .iter()
            .any(|(key, text)| key == "build" && text == "passing"),
        "latest set wins; got {statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .any(|(key, text)| key == "queue" && text == "2"),
        "distinct keys are kept; got {statuses:?}"
    );

    // Clearing removes the cell from the host view.
    status.set_status("build", None);
    let statuses = session.plugin_statuses();
    assert!(
        statuses.iter().all(|(key, _)| key != "build"),
        "cleared cell must disappear; got {statuses:?}"
    );
    // A stale context must not write into the live cell: rebinding is the
    // host's only invalidation point, and this session never rebinds.
    assert!(ctx.is_active());
}

// ---------------------------------------------------------------------------
// (d) event bus: FIFO, no drops under a bounded burst
// ---------------------------------------------------------------------------

#[test]
fn event_bus_delivers_fifo_without_drops_under_bounded_burst() {
    let temp = TempDir::new("events");
    let plugin = ProbePlugin::new("live-event-probe");
    let (_session, plugin) = live_session(&temp, &plugin);
    let ctx = plugin.last_context();
    let events = ctx.require_events().expect("event bus");

    let received: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&received);
    let subscription = events.subscribe(
        "probe.channel",
        Arc::new(move |payload: &JsonValue| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(payload["n"].as_u64().expect("n"));
        }),
    );
    // A different channel must not receive these events.
    events.emit("other.channel", json!({ "n": 9999 }));

    const BURST: u64 = 512;
    for n in 0..BURST {
        events.emit("probe.channel", json!({ "n": n }));
    }
    let received_values = received
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        received_values.len(),
        BURST as usize,
        "a synchronous bounded burst is never dropped"
    );
    assert_eq!(
        received_values,
        (0..BURST).collect::<Vec<_>>(),
        "per-subscriber delivery is FIFO"
    );

    subscription.cancel();
    events.emit("probe.channel", json!({ "n": BURST }));
    assert_eq!(
        received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        BURST as usize,
        "cancel stops delivery"
    );
}

// ---------------------------------------------------------------------------
// (e) rebinding bumps the epoch; the old context is stale
// ---------------------------------------------------------------------------

#[test]
fn rebinding_bumps_epoch_and_stales_the_old_context() {
    let plugin = ProbePlugin::new("rebind-probe");
    let host = PluginHost::build(None, vec![plugin.clone() as Arc<dyn AgentPlugin>]);

    host.bind_context(PluginServices::new().build());
    let first = plugin.last_context();
    assert!(first.is_active());
    let first_epoch = first.epoch();

    host.bind_context(PluginServices::new().build());
    let second = plugin.last_context();
    assert_eq!(
        plugin.context_count(),
        2,
        "each bind is one handoff per plugin"
    );
    assert_eq!(second.epoch(), first_epoch + 1);
    assert_eq!(host.context_epoch(), second.epoch());
    assert!(second.is_active());

    // The cached old context reports staleness with both epochs.
    assert!(!first.is_active());
    match first.check_active() {
        Err(PluginServiceError::Stale { epoch, current }) => {
            assert_eq!(epoch, first_epoch);
            assert_eq!(current, second.epoch());
        }
        other => panic!("expected Stale, got {other:?}"),
    }
}
