//! Hardening tests for the live native plugin services.
//!
//! Pins the four fixes from the adversarial review's live-service findings:
//!
//! 1. `EventService::emit` isolates a panicking subscriber (later subscribers
//!    still run, a later emit still delivers).
//! 2. `PluginHost::bind_context` serializes epoch bump + store + notify, so a
//!    concurrent bind cannot leave the stored context behind the live epoch.
//! 3. `ProviderService::has_auth` serves its cached answer without touching
//!    disk and refreshes off the caller's thread after the TTL; the host's
//!    `live_auth_disk_reads` probe counts the reads.
//! 4. `SessionService::all_tools`/`commands` memoize the typed `Arc` per
//!    registry generation (`Arc::ptr_eq` on repeated reads; a rebuild
//!    invalidates the memo).
//!
//! Plus a status-service reentrancy check: a bus callback may read/write
//! statuses and call the host's merge path while the emit is in flight.
//!
//! In-memory only: scripted providers that are never streamed, temp session
//! dirs, no extension runtime, and no network.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value as JsonValue};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{Context, Model, ModelCost, ModelCostRates, StreamOptions, ThinkingLevel};
use pir::plugins::plugin::PluginContext;
use pir::plugins::services::{PluginServices, SessionService};
use pir::plugins::{AgentPlugin, PluginHost, PluginInfo};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A native probe plugin: records every context the host hands it.
struct ContextProbe {
    info: PluginInfo,
    contexts: Mutex<Vec<PluginContext>>,
}

impl ContextProbe {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
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

impl AgentPlugin for ContextProbe {
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

/// In-memory provider; its model list backs the session's typed model reads.
struct TestProvider {
    id: String,
    model: Model,
}

impl TestProvider {
    fn new(id: &str, model_id: &str) -> Self {
        Self {
            id: id.to_owned(),
            model: model_for(id, model_id),
        }
    }
}

impl Provider for TestProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.id
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
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

fn model_for(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_owned(),
        name: id.to_owned(),
        api: "openai-completions".to_owned(),
        provider: provider.to_owned(),
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

fn registry_with(providers: Vec<TestProvider>) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    for provider in providers {
        registry.set_provider(Arc::new(provider));
    }
    Arc::new(registry)
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-plugin-hardening-{label}-{}-{}",
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
    registry: Arc<ProviderRegistry>,
    model: Model,
    tools: ToolRegistry,
) -> AgentSessionConfig {
    let cwd = temp.path().to_str().expect("utf-8 temp path").to_owned();
    let session_dir = temp.path().join("sessions");
    AgentSessionConfig {
        provider_registry: registry,
        model,
        api_key: None,
        thinking: ThinkingLevel::High,
        tools,
        session: Some(SessionManager::create(&cwd, &session_dir).expect("create session")),
        compaction: None,
        max_turns: 4,
        system_prompt: Some("plugin services hardening prompt".to_owned()),
        cwd: Some(temp.path().to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

fn live_session(temp: &TempDir, plugin: Arc<dyn AgentPlugin>) -> Arc<AgentSession> {
    AgentSession::new_arc_with_plugins(
        session_config(
            temp,
            registry_with(vec![TestProvider::new("fake", "model-a")]),
            model_for("fake", "model-a"),
            ToolRegistry::new(),
        ),
        vec![plugin],
    )
}

// ---------------------------------------------------------------------------
// 1. Event bus: a panicking subscriber is isolated
// ---------------------------------------------------------------------------

#[test]
fn panicking_event_subscriber_does_not_break_the_bus() {
    let temp = TempDir::new("event-panic");
    let probe = ContextProbe::new("event-panic-probe");
    let _session = live_session(&temp, probe.clone() as Arc<dyn AgentPlugin>);
    let ctx = probe.last_context();
    let events = ctx.events_arc().expect("event bus bound");

    let first = Arc::new(Mutex::new(Vec::<u64>::new()));
    let middle = Arc::new(Mutex::new(Vec::<u64>::new()));
    let last = Arc::new(Mutex::new(Vec::<u64>::new()));
    let middle_calls = Arc::new(AtomicUsize::new(0));

    let sink = Arc::clone(&first);
    let _first_sub = events.subscribe(
        "panic.channel",
        Arc::new(move |payload: &JsonValue| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(payload["n"].as_u64().expect("n"));
        }),
    );
    let sink = Arc::clone(&middle);
    let calls = Arc::clone(&middle_calls);
    let _middle_sub = events.subscribe(
        "panic.channel",
        Arc::new(move |payload: &JsonValue| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                panic!("middle subscriber panics on the first emit");
            }
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(payload["n"].as_u64().expect("n"));
        }),
    );
    let sink = Arc::clone(&last);
    let _last_sub = events.subscribe(
        "panic.channel",
        Arc::new(move |payload: &JsonValue| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(payload["n"].as_u64().expect("n"));
        }),
    );

    events.emit("panic.channel", json!({ "n": 0 }));

    assert_eq!(
        first
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        vec![0],
        "the subscriber before the panicking one still ran"
    );
    assert_eq!(
        last.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        vec![0],
        "the subscriber after the panicking one still ran"
    );

    // The bus is not poisoned and the subscription list is intact: the
    // formerly panicking subscriber now receives too.
    events.emit("panic.channel", json!({ "n": 1 }));
    assert_eq!(
        first
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        vec![0, 1]
    );
    assert_eq!(
        middle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        vec![1],
        "the panicking subscriber is called again on the next emit"
    );
    assert_eq!(
        last.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        vec![0, 1]
    );
}

// ---------------------------------------------------------------------------
// 2. bind_context: concurrent binds are serialized
// ---------------------------------------------------------------------------

/// Marker session service so a test can tell which context was stored.
struct MarkerSession {
    cwd: Arc<Path>,
}

impl SessionService for MarkerSession {
    fn cwd(&self) -> Option<Arc<Path>> {
        Some(Arc::clone(&self.cwd))
    }
}

fn services_with_cwd(path: &str) -> PluginContext {
    PluginServices::new()
        .with_session(Arc::new(MarkerSession {
            cwd: Arc::from(Path::new(path)),
        }))
        .build()
}

/// Blocks the first `on_context` until released and records every epoch.
struct GatedBindPlugin {
    info: PluginInfo,
    calls: AtomicUsize,
    epochs: Mutex<Vec<u64>>,
    entered: mpsc::Sender<usize>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl GatedBindPlugin {
    fn new(name: &str, entered: mpsc::Sender<usize>, release: mpsc::Receiver<()>) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            calls: AtomicUsize::new(0),
            epochs: Mutex::new(Vec::new()),
            entered,
            release: Mutex::new(release),
        })
    }

    fn epochs(&self) -> Vec<u64> {
        self.epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl AgentPlugin for GatedBindPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn on_context(&self, ctx: &PluginContext) {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.epoch());
        let _ = self.entered.send(call);
        if call == 1 {
            // Keep the first bind inside its store+notify window while the
            // test starts a second bind.
            let _ = self
                .release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv();
        }
    }
}

#[test]
fn concurrent_binds_are_serialized_and_store_the_newest_context() {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let plugin = GatedBindPlugin::new("bind-gate", entered_tx, release_rx);
    let host = Arc::new(PluginHost::build(
        None,
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    ));

    let host_a = Arc::clone(&host);
    let first = std::thread::spawn(move || {
        host_a.bind_context(services_with_cwd("C:/hardening-bind-a"));
    });
    assert_eq!(
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first bind entered on_context"),
        1
    );

    let (started_tx, started_rx) = mpsc::channel();
    let host_b = Arc::clone(&host);
    let second = std::thread::spawn(move || {
        let _ = started_tx.send(());
        host_b.bind_context(services_with_cwd("C:/hardening-bind-b"));
    });
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second bind thread started");
    // Without the bind lock the second bind bumps the epoch and stores while
    // the first is still inside on_context.
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        host.context_epoch(),
        1,
        "a concurrent bind must wait for the first bind's store+notify window"
    );

    release_tx.send(()).expect("release the first bind");
    first.join().expect("first bind thread");
    second.join().expect("second bind thread");

    assert_eq!(host.context_epoch(), 2);
    assert_eq!(plugin.epochs(), vec![1, 2], "on_context ran in epoch order");
    let bound = host.context().expect("context bound");
    assert_eq!(bound.epoch(), 2);
    assert!(bound.is_active(), "the stored context is the live epoch");
    let cwd = bound
        .session()
        .and_then(|session| session.cwd())
        .expect("stored session service");
    assert_eq!(
        cwd.as_ref(),
        Path::new("C:/hardening-bind-b"),
        "the newest bind's complete context is the one stored"
    );
}

// ---------------------------------------------------------------------------
// 3. has_auth: warm cache, async refresh, counted disk reads
// ---------------------------------------------------------------------------

static AGENT_DIR_LOCK: Mutex<()> = Mutex::new(());

/// Pins the process-global `PIR_AGENT_DIR` for the auth-refresh test and
/// restores it (while still holding the lock) on drop.
struct AgentDirGuard {
    previous: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl AgentDirGuard {
    fn pin(dir: &Path) -> Self {
        let lock = AGENT_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os("PIR_AGENT_DIR");
        std::env::set_var("PIR_AGENT_DIR", dir);
        Self {
            previous,
            _lock: lock,
        }
    }
}

impl Drop for AgentDirGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
            None => std::env::remove_var("PIR_AGENT_DIR"),
        }
    }
}

#[test]
fn has_auth_serves_warm_cache_and_refreshes_in_the_background() {
    let temp = TempDir::new("auth-refresh");
    let agent_dir = temp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let _env = AgentDirGuard::pin(&agent_dir);

    // Unique ids so the host environment can never answer for them.
    let alpha = format!("hardening-alpha-{}", std::process::id());
    let beta = format!("hardening-beta-{}", std::process::id());
    let registry = registry_with(vec![
        TestProvider::new(&alpha, "alpha-model"),
        TestProvider::new(&beta, "beta-model"),
    ]);
    let probe = ContextProbe::new("auth-probe");
    let _session = AgentSession::new_arc_with_plugins(
        session_config(
            &temp,
            registry,
            model_for(&alpha, "alpha-model"),
            ToolRegistry::new(),
        ),
        vec![probe.clone() as Arc<dyn AgentPlugin>],
    );
    let ctx = probe.last_context();
    let providers = ctx.providers().expect("provider service bound");

    pir::plugins::host::reset_live_auth_disk_reads();
    // Cold call: one synchronous read, empty API-key set => permissive true.
    assert!(providers.has_auth(&alpha));
    assert!(providers.has_auth(&beta));
    assert_eq!(
        pir::plugins::host::live_auth_disk_reads(),
        1,
        "the cold call performs exactly one credential-environment read"
    );

    // N warm calls: no disk.
    for _ in 0..64 {
        let _ = providers.has_auth(&alpha);
        let _ = providers.has_auth(&beta);
    }
    assert_eq!(
        pir::plugins::host::live_auth_disk_reads(),
        1,
        "warm calls never touch disk"
    );

    // Grant alpha a key. The cached snapshot is still served within the TTL.
    std::fs::write(
        agent_dir.join("models.json"),
        format!(r#"{{"providers":{{"{alpha}":{{"apiKey":"hardening-key"}}}}}}"#),
    )
    .expect("write models.json");
    assert!(
        providers.has_auth(&beta),
        "a warm (unexpired) call serves the cached snapshot"
    );
    assert_eq!(pir::plugins::host::live_auth_disk_reads(), 1);

    // Expire the TTL: the call itself must return the stale answer without a
    // synchronous read; the refresh lands in the background.
    std::thread::sleep(Duration::from_millis(1100));
    assert!(
        providers.has_auth(&beta),
        "the expired call returns the stale snapshot immediately"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while pir::plugins::host::live_auth_disk_reads() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        pir::plugins::host::live_auth_disk_reads() >= 2,
        "a background refresh ran after the TTL"
    );

    // The refresh publishes alpha as authenticated, so beta flips to false.
    while providers.has_auth(&beta) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        providers.has_auth(&alpha),
        "alpha has a key after the refresh"
    );
    assert!(
        !providers.has_auth(&beta),
        "the background refresh published the new credential set"
    );
    assert_eq!(
        pir::plugins::host::live_auth_disk_reads(),
        2,
        "exactly one background read per TTL window"
    );
}

// ---------------------------------------------------------------------------
// 4. all_tools/commands: memoized typed Arcs per registry generation
// ---------------------------------------------------------------------------

/// Core tool whose schema builder counts calls.
struct CountingCoreTool {
    parameters_calls: Arc<AtomicUsize>,
    output_schema_calls: Arc<AtomicUsize>,
}

impl AgentTool for CountingCoreTool {
    fn name(&self) -> &'static str {
        "hardening-core"
    }

    fn description(&self) -> &'static str {
        "core tool for the all_tools memo test"
    }

    fn parameters(&self) -> Option<JsonValue> {
        self.parameters_calls.fetch_add(1, Ordering::SeqCst);
        Some(json!({ "type": "object", "properties": {} }))
    }

    fn output_schema(&self) -> Option<JsonValue> {
        self.output_schema_calls.fetch_add(1, Ordering::SeqCst);
        None
    }

    fn run(&self, _args: &JsonValue, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

/// Plugin tool whose description is captured at registration time.
struct VersionedPluginTool {
    v2: bool,
}

impl AgentTool for VersionedPluginTool {
    fn name(&self) -> &'static str {
        "hardening-plugin-tool"
    }

    fn description(&self) -> &'static str {
        if self.v2 {
            "plugin tool v2"
        } else {
            "plugin tool v1"
        }
    }

    fn parameters(&self) -> Option<JsonValue> {
        Some(json!({ "type": "object", "properties": {} }))
    }

    fn run(&self, _args: &JsonValue, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

/// Plugin that registers one tool and one command; `version` selects the
/// definitions the next registry build publishes.
struct CacheProbePlugin {
    info: PluginInfo,
    version: AtomicUsize,
    contexts: Mutex<Vec<PluginContext>>,
}

impl CacheProbePlugin {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            version: AtomicUsize::new(0),
            contexts: Mutex::new(Vec::new()),
        })
    }

    fn set_v2(&self) {
        self.version.store(1, Ordering::SeqCst);
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

impl AgentPlugin for CacheProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut pir::plugins::RegistrationContext<'_>) -> anyhow::Result<()> {
        let v2 = self.version.load(Ordering::SeqCst) == 1;
        ctx.register_tool(Arc::new(VersionedPluginTool { v2 }));
        ctx.register_command(
            "hardening-cache-cmd",
            Some(if v2 { "command v2" } else { "command v1" }),
            None,
        );
        Ok(())
    }

    fn on_context(&self, ctx: &PluginContext) {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.clone());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn all_tools_and_commands_memoize_per_registry_generation() {
    let temp = TempDir::new("tool-cache");
    let parameters_calls = Arc::new(AtomicUsize::new(0));
    let output_schema_calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(CountingCoreTool {
        parameters_calls: Arc::clone(&parameters_calls),
        output_schema_calls: Arc::clone(&output_schema_calls),
    }));

    let plugin = CacheProbePlugin::new("tool-cache-probe");
    let session = AgentSession::new_arc_with_plugins(
        session_config(
            &temp,
            registry_with(vec![TestProvider::new("fake", "model-a")]),
            model_for("fake", "model-a"),
            tools,
        ),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    let ctx = plugin.last_context();
    let service = ctx.session().expect("session service bound");

    let before = parameters_calls.load(Ordering::SeqCst);
    let first_tools = service.all_tools();
    let first_commands = service.commands();
    assert_eq!(
        parameters_calls.load(Ordering::SeqCst),
        before + 1,
        "the first read materializes the core schemas once"
    );
    assert!(
        first_tools
            .iter()
            .any(|def| def.name == "hardening-plugin-tool" && def.description == "plugin tool v1"),
        "the plugin tool is in the merged view"
    );
    assert!(
        first_commands
            .iter()
            .any(|def| def.name == "hardening-cache-cmd"
                && def.description.as_deref() == Some("command v1")),
        "the plugin command is in the merged view"
    );

    // Warm reads: same allocation, no schema rebuild.
    let second_tools = service.all_tools();
    let second_commands = service.commands();
    assert!(
        Arc::ptr_eq(&first_tools, &second_tools),
        "an unchanged registry generation returns the same Arc"
    );
    assert!(
        Arc::ptr_eq(&first_commands, &second_commands),
        "commands are memoized too"
    );
    assert_eq!(
        parameters_calls.load(Ordering::SeqCst),
        before + 1,
        "warm reads do not call parameters() again"
    );
    assert_eq!(
        output_schema_calls.load(Ordering::SeqCst),
        0,
        "the read path never calls output_schema()"
    );

    // A rebuild publishes new definitions and invalidates both memos.
    plugin.set_v2();
    session
        .plugin_host()
        .expect("session plugin host")
        .rebuild_registry()
        .await;

    let third_tools = service.all_tools();
    let third_commands = service.commands();
    assert!(
        !Arc::ptr_eq(&first_tools, &third_tools),
        "a rebuilt registry returns a new Arc"
    );
    assert!(
        !Arc::ptr_eq(&first_commands, &third_commands),
        "a rebuilt registry invalidates the command memo"
    );
    assert!(
        third_tools
            .iter()
            .any(|def| def.name == "hardening-plugin-tool" && def.description == "plugin tool v2"),
        "the new tool definition is reflected"
    );
    assert!(
        third_commands
            .iter()
            .any(|def| def.name == "hardening-cache-cmd"
                && def.description.as_deref() == Some("command v2")),
        "the new command definition is reflected"
    );
    assert_eq!(
        parameters_calls.load(Ordering::SeqCst),
        before + 2,
        "one rebuild costs exactly one materialization"
    );

    // The new generation is memoized in turn.
    let fourth_tools = service.all_tools();
    assert!(Arc::ptr_eq(&third_tools, &fourth_tools));
    assert_eq!(parameters_calls.load(Ordering::SeqCst), before + 2);
}

// ---------------------------------------------------------------------------
// 5. StatusService: leaf lock, safe to read from a callback
// ---------------------------------------------------------------------------

#[test]
fn event_callback_can_read_and_write_statuses_without_deadlock() {
    let temp = TempDir::new("status-reentrant");
    let probe = ContextProbe::new("status-reentrant-probe");
    let session = live_session(&temp, probe.clone() as Arc<dyn AgentPlugin>);
    let ctx = probe.last_context();
    let events = ctx.events_arc().expect("event bus bound");
    let status = ctx.status_arc().expect("status service bound");

    let status_in_callback = Arc::clone(&status);
    // Weak: the bus lives inside the session's own services, so an `Arc` here
    // would close the session -> host -> plugin -> context -> bus -> session
    // cycle.
    let session_in_callback = Arc::downgrade(&session);
    let _subscription = events.subscribe(
        "status.channel",
        Arc::new(move |_payload: &JsonValue| {
            // Write, read, snapshot, then run the host's merge path: all of
            // these take the status service's leaf mutex in sequence, never
            // while it is already held.
            status_in_callback.set_status("callback", Some("1"));
            assert_eq!(status_in_callback.status("callback").as_deref(), Some("1"));
            assert!(status_in_callback
                .statuses()
                .iter()
                .any(|(key, text)| key.as_ref() == "callback" && text.as_ref() == "1"));
            let session = session_in_callback.upgrade().expect("session alive");
            assert!(
                session
                    .plugin_statuses()
                    .iter()
                    .any(|(key, text)| key == "callback" && text == "1"),
                "the host merge reads the same cells while the emit is in flight"
            );
        }),
    );

    events.emit("status.channel", json!({}));
    assert!(session
        .plugin_statuses()
        .iter()
        .any(|(key, text)| key == "callback" && text == "1"));
}
