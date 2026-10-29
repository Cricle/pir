//! Release-mode service-performance test for the native plugin service layer.
//!
//! # Run
//!
//! ```text
//! cargo test --release --features phone --test plugin_service_perf -- --ignored --nocapture
//! ```
//!
//! Every test is `#[ignore = "perf: run in release"]`, so the normal suite does
//! not pay for the measurements. `run_all()` runs every scenario; the individual
//! `#[ignore]`d tests each run one scenario.
//!
//! # What is measured
//!
//! Each timed closure goes through the real Rust face:
//! `PluginContext::<service>()` (an `Option<&dyn Trait>` field read) followed by
//! one call through the trait object. The concrete service type never appears
//! on the measured path. The budgets come from
//! `logs/perf/agent-parity-audit.md` §4.3:
//!
//! | Scenario | Budget |
//! |---|---|
//! | status set / get | 200 ns/call each |
//! | session entries `Arc` read | 100 ns/call |
//! | event-bus emit, 1 subscriber | 5 us/call |
//! | tool-registry lookup/dispatch | 2 us/call |
//! | dialog/UI action enqueue | 10 us/call |
//! | renderer/factory cache hit | 1 us/call |
//!
//! Assertions use twice the stated budget (plus a small integer-nanosecond
//! floor for the direct-vs-service ratio) so a loaded Windows CI box does not
//! flake; the raw medians printed below are the deliverable.
//!
//! # Live vs fake backends
//!
//! - **Live** (`src/agent_session/plugin_services.rs`): `status_set_get` and
//!   `event_emit` read the `PluginContext` bound by a real
//!   `AgentSession::new_arc_with_plugins`, i.e. the production
//!   `NativeStatusService` (insertion-ordered `Arc<str>` cells) and
//!   `NativeEventBus` (synchronous per-channel fan-out).
//! - **Fake** (labelled in every printed line): `entries_arc`,
//!   `tool_dispatch`, `dialog_enqueue`, `renderer_cache`, and the direct side
//!   of `direct_vs_service`. The live adapter deliberately returns `None` for
//!   `entries()` (no host-retained snapshot `Arc` exists yet) and binds no
//!   tool/UI/renderer service, so a fake is the honest way to measure those
//!   trait paths. The fakes use the real `Arc`/trait shapes (`Arc<JsonValue>`,
//!   `ToolRegistry` + `ToolInfo`, `VecDeque` action queue, renderer map).
//!
//! The live fixtures are hermetic: the session manager writes only under
//! `std::env::temp_dir()`, and the session is built with `extensions: None` and
//! an inline probe plugin, so no `~/.pi` configuration is read or written.
//! Zero re-serialization is pinned directly: `entries()` must return the same
//! `Arc` pointer on every call (`Arc::ptr_eq`).
//!
//! The audit's Rust-vs-JS ratio (Rust should be >=3-10x the QuickJS/JSON hop)
//! is not re-measured here: this target has no extension runtime, and these
//! Rust medians are the baseline the cross-face perf job compares against. The
//! zero-re-serialization half of that gate is the `Arc::ptr_eq` assertion.
//!
//! # Timer
//!
//! `median_ns(label, iters_per_sample, f)` warms up with [`WARMUP_ITERS`]
//! calls, then times [`SAMPLES`] samples of `iters_per_sample` calls each and
//! prints the median as `label: <n> ns/call`. `10_000` iterations per sample is
//! used for ns-scale work and `1_000` for us-scale work. No external timing
//! crate is used.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value as JsonValue};

use pir::agent::nested_tool_calls::NestedToolOutcome;
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{Context, Model, ModelCost, ModelCostRates, StreamOptions, ThinkingLevel};
use pir::extensions::RendererOutcome;
use pir::plugins::plugin::services::{
    DialogOptions, ExecuteToolOptions, MessageRenderer, PluginServiceError, RendererKind,
    RendererService, SelectOption, ServiceFuture, ServiceResult, SessionService, StatusEntry,
    StatusService, ToolInfo, ToolService, UiService,
};
use pir::plugins::plugin::{PluginContext, PluginServices};
use pir::plugins::{AgentPlugin, PluginInfo};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Median-of-batches timer
// ---------------------------------------------------------------------------

/// Calls per sample for ns-scale work.
const NS_ITERS: u32 = 10_000;
/// Calls per sample for us-scale work.
const US_ITERS: u32 = 1_000;
/// Samples per measurement; the median is the middle of the sorted samples.
const SAMPLES: usize = 7;
/// Untimed calls before the first sample.
const WARMUP_ITERS: u32 = 2_000;

// Audit budgets (ns) and the CI assertion multiplier.
const STATUS_BUDGET_NS: u128 = 200;
const ENTRIES_BUDGET_NS: u128 = 100;
const EVENT_BUDGET_NS: u128 = 5_000;
const TOOL_BUDGET_NS: u128 = 2_000;
const DIALOG_BUDGET_NS: u128 = 10_000;
const RENDERER_BUDGET_NS: u128 = 1_000;
const CI_BUDGET_MULTIPLIER: u128 = 2;

/// Warm up, time `SAMPLES` batches, print and return the median ns/call.
fn median_ns<T>(label: &str, iters_per_sample: u32, mut f: impl FnMut() -> T) -> u128 {
    for _ in 0..WARMUP_ITERS {
        std::hint::black_box(f());
    }
    let mut samples = [0u128; SAMPLES];
    for sample in &mut samples {
        let start = Instant::now();
        for _ in 0..iters_per_sample {
            std::hint::black_box(f());
        }
        *sample = start.elapsed().as_nanos() / u128::from(iters_per_sample);
    }
    samples.sort_unstable();
    let median = samples[SAMPLES / 2];
    println!("{label}: {median} ns/call (median of {SAMPLES} samples x {iters_per_sample} iters)");
    median
}

// ---------------------------------------------------------------------------
// Live fixture: a real AgentSession-bound PluginContext
// ---------------------------------------------------------------------------

/// Captures the `PluginContext` the live session binds.
struct CapturingPlugin {
    info: PluginInfo,
    contexts: Mutex<Vec<PluginContext>>,
}

impl CapturingPlugin {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline("perf-service-probe"),
            contexts: Mutex::new(Vec::new()),
        })
    }

    fn last_context(&self) -> PluginContext {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .expect("the live session bound a plugin context")
    }
}

impl AgentPlugin for CapturingPlugin {
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

/// In-memory provider; its model list backs the live session's typed reads.
/// The session never prompts, so `stream` is never driven.
struct FakeProvider;

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "perf-fake"
    }

    fn name(&self) -> &str {
        "Perf fake provider"
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
        id: "perf-model".to_owned(),
        name: "Perf Model".to_owned(),
        api: "openai-completions".to_owned(),
        provider: "perf-fake".to_owned(),
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

fn fake_registry() -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider));
    Arc::new(registry)
}

/// Temp directory owned by one live fixture (session storage only).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-plugin-service-perf-{label}-{}-{}",
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
        system_prompt: Some("plugin service perf test".to_owned()),
        cwd: Some(temp.path().to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

/// Build a hermetic `AgentSession` and return its bound live `PluginContext`.
/// The returned temp dir and session keep the services alive for the test.
fn live_context(label: &str) -> (TempDir, Arc<AgentSession>, PluginContext) {
    let temp = TempDir::new(label);
    let plugin = CapturingPlugin::new();
    let session = AgentSession::new_arc_with_plugins(
        session_config(&temp),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    let ctx = plugin.last_context();
    (temp, session, ctx)
}

// ---------------------------------------------------------------------------
// Fakes for services the live adapter does not bind
// ---------------------------------------------------------------------------

/// Fake session backed by one shared `Arc<JsonValue>` cell: the same
/// zero-copy shape a host-retained snapshot would have. The inherent
/// `direct_entries` method is the direct-core side of the ratio check.
struct PerfEntriesSession {
    entries_cell: Arc<JsonValue>,
}

impl PerfEntriesSession {
    fn new(entries: JsonValue) -> Self {
        Self {
            entries_cell: Arc::new(entries),
        }
    }

    #[inline]
    fn direct_entries(&self) -> Option<Arc<JsonValue>> {
        Some(Arc::clone(&self.entries_cell))
    }
}

impl SessionService for PerfEntriesSession {
    fn entries(&self) -> Option<Arc<JsonValue>> {
        Some(Arc::clone(&self.entries_cell))
    }
}

/// Fake status cells with the same insertion-ordered `Vec<(Arc<str>, Arc<str>)>`
/// shape as the live `NativeStatusService`. The inherent methods are the
/// direct-core side of the status ratio check.
#[derive(Default)]
struct PerfStatusService {
    cells: Mutex<Vec<StatusEntry>>,
}

impl PerfStatusService {
    fn direct_set_status(&self, key: &str, text: Option<&str>) {
        let mut cells = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match cells.iter().position(|(cell, _)| cell.as_ref() == key) {
            Some(index) => match text {
                Some(text) => cells[index].1 = Arc::from(text),
                None => {
                    cells.remove(index);
                }
            },
            None => {
                if let Some(text) = text {
                    cells.push((Arc::from(key), Arc::from(text)));
                }
            }
        }
    }

    fn direct_status(&self, key: &str) -> Option<Arc<str>> {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(cell, _)| cell.as_ref() == key)
            .map(|(_, text)| Arc::clone(text))
    }
}

impl StatusService for PerfStatusService {
    fn set_status(&self, key: &str, text: Option<&str>) {
        self.direct_set_status(key, text);
    }

    fn statuses(&self) -> Arc<[StatusEntry]> {
        Arc::from(
            self.cells
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
    }

    fn status(&self, key: &str) -> Option<Arc<str>> {
        self.direct_status(key)
    }
}

/// Fake dialog queue: one id allocation plus one bounded enqueue per call, the
/// same "queue an action, return a reply later" shape as the real UI service.
const DIALOG_QUEUE_CAP: usize = 1_024;

struct PerfUiService {
    next_id: AtomicU64,
    queue: Mutex<VecDeque<(u64, String)>>,
}

impl Default for PerfUiService {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(0),
            queue: Mutex::new(VecDeque::new()),
        }
    }
}

impl UiService for PerfUiService {
    fn select(
        &self,
        title: &str,
        _options: &[SelectOption],
        _dialog: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<String>>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.len() >= DIALOG_QUEUE_CAP {
            queue.pop_front();
        }
        queue.push_back((id, title.to_owned()));
        Box::pin(std::future::ready(Ok(Some("ok".to_owned()))))
    }
}

/// A registry target for the fake `ToolService` lookup.
struct PerfNoopTool;

impl AgentTool for PerfNoopTool {
    fn name(&self) -> &'static str {
        "perf-noop"
    }

    fn description(&self) -> &'static str {
        "no-op perf dispatch target"
    }

    fn run(&self, _args: &JsonValue, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        unimplemented!("the perf fake never executes the tool body")
    }
}

/// Fake tool service over the real `ToolRegistry`: `execute_tool` performs the
/// registry lookup and reply admission synchronously, then returns a ready
/// future. A no-op tool has no body work, so the future is never polled.
struct PerfToolService {
    registry: ToolRegistry,
    callable: Arc<[ToolInfo]>,
}

impl PerfToolService {
    fn new() -> Self {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(PerfNoopTool));
        let callable: Arc<[ToolInfo]> = Arc::from(vec![tool_info("perf-noop")]);
        Self { registry, callable }
    }
}

fn tool_info(name: &str) -> ToolInfo {
    serde_json::from_value(json!({
        "name": name,
        "description": "no-op perf dispatch target",
        "parameters": { "type": "object" },
    }))
    .expect("ToolDef deserializes from its wire shape")
}

impl ToolService for PerfToolService {
    fn callable_tools(&self) -> Arc<[ToolInfo]> {
        Arc::clone(&self.callable)
    }

    fn execute_tool(
        &self,
        caller_id: &str,
        name: &str,
        _args: JsonValue,
        _options: ExecuteToolOptions,
    ) -> ServiceFuture<'static, ServiceResult<NestedToolOutcome>> {
        let result: ServiceResult<NestedToolOutcome> = match self.registry.get(name) {
            Some(tool) => Ok(NestedToolOutcome {
                tool_call_id: format!("{caller_id}/1"),
                tool_name: tool.name().to_owned(),
                result: ToolResult::ok("ok"),
                is_error: false,
                duration_ms: Some(0),
                usage: None,
            }),
            None => Err(PluginServiceError::unavailable("tools")),
        };
        Box::pin(std::future::ready(result))
    }
}

/// Fake renderer registry with a `(kind, key)` map lookup; `has` is the TUI's
/// synchronous cache-hit pre-check.
struct PerfRendererService {
    message: Mutex<HashMap<String, Arc<dyn MessageRenderer>>>,
}

impl Default for PerfRendererService {
    fn default() -> Self {
        Self {
            message: Mutex::new(HashMap::new()),
        }
    }
}

impl RendererService for PerfRendererService {
    fn register_message(&self, custom_type: &str, renderer: Arc<dyn MessageRenderer>) {
        self.message
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(custom_type.to_owned(), renderer);
    }

    fn has(&self, kind: RendererKind, key: &str) -> bool {
        match kind {
            RendererKind::Message => self
                .message
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(key),
            _ => false,
        }
    }
}

struct NoopRenderer;

impl MessageRenderer for NoopRenderer {
    fn render(&self, _message: &JsonValue, _width: usize) -> RendererOutcome {
        RendererOutcome::default()
    }
}

// ---------------------------------------------------------------------------
// Scenario 1: status set/get through the live StatusService
// ---------------------------------------------------------------------------

fn measure_status_set_get() {
    let (_temp, _session, ctx) = live_context("status");

    let set_ns = median_ns("status_set (live NativeStatusService)", NS_ITERS, || {
        ctx.status()
            .expect("live status service")
            .set_status("perf.status", Some("running"))
    });
    let get_ns = median_ns("status_get (live NativeStatusService)", NS_ITERS, || {
        ctx.status()
            .expect("live status service")
            .status("perf.status")
    });

    assert!(
        set_ns <= STATUS_BUDGET_NS * CI_BUDGET_MULTIPLIER,
        "status_set median {set_ns} ns/call exceeds {} ns (2 x {STATUS_BUDGET_NS} ns budget)",
        STATUS_BUDGET_NS * CI_BUDGET_MULTIPLIER
    );
    assert!(
        get_ns <= STATUS_BUDGET_NS * CI_BUDGET_MULTIPLIER,
        "status_get median {get_ns} ns/call exceeds {} ns (2 x {STATUS_BUDGET_NS} ns budget)",
        STATUS_BUDGET_NS * CI_BUDGET_MULTIPLIER
    );
}

// ---------------------------------------------------------------------------
// Scenario 2: session entries Arc read
// ---------------------------------------------------------------------------

fn entries_payload() -> JsonValue {
    // 500 entries: the audit's reference session size. The read must stay flat
    // because it is an Arc clone, never a serialize/parse of this payload.
    JsonValue::Array(
        (0..500)
            .map(|index| {
                json!({
                    "type": "message",
                    "id": format!("entry-{index}"),
                    "role": "assistant",
                    "content": "perf payload",
                })
            })
            .collect(),
    )
}

fn measure_entries_arc() {
    let fake = Arc::new(PerfEntriesSession::new(entries_payload()));
    let ctx = PluginServices::new()
        .with_session(fake.clone() as Arc<dyn SessionService>)
        .build();

    let first = ctx
        .session()
        .expect("session service")
        .entries()
        .expect("entries cell");
    let second = ctx
        .session()
        .expect("session service")
        .entries()
        .expect("entries cell");
    assert!(
        Arc::ptr_eq(&first, &second),
        "two entries() reads must return the same Arc (no re-serialization)"
    );
    assert!(
        Arc::ptr_eq(&first, &fake.entries_cell),
        "the service must return the host-held Arc, not a copy"
    );

    let ns = median_ns(
        "entries_arc (fake SessionService, 500-entry Arc)",
        NS_ITERS,
        || ctx.session().expect("session service").entries(),
    );
    assert!(
        ns <= ENTRIES_BUDGET_NS * CI_BUDGET_MULTIPLIER,
        "entries read median {ns} ns/call exceeds {} ns (2 x {ENTRIES_BUDGET_NS} ns budget)",
        ENTRIES_BUDGET_NS * CI_BUDGET_MULTIPLIER
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: event-bus emit through the live EventService
// ---------------------------------------------------------------------------

fn measure_event_emit() {
    let (_temp, _session, ctx) = live_context("events");
    let payload = json!({ "channel": "perf", "n": 1 });
    let delivered = Arc::new(AtomicUsize::new(0));
    let sink = Arc::clone(&delivered);
    let subscription = ctx.events().expect("live event bus").subscribe(
        "perf.events",
        Arc::new(move |_payload: &JsonValue| {
            sink.fetch_add(1, Ordering::Relaxed);
        }),
    );

    let ns = median_ns(
        "event_emit (live NativeEventBus, 1 subscriber)",
        US_ITERS,
        || {
            ctx.events()
                .expect("live event bus")
                .emit("perf.events", payload.clone())
        },
    );
    subscription.cancel();

    // Warm-up + every timed emit must reach the single subscriber.
    let expected = WARMUP_ITERS as usize + SAMPLES * US_ITERS as usize;
    assert_eq!(
        delivered.load(Ordering::Relaxed),
        expected,
        "every bus emit must reach the one subscriber"
    );
    assert!(
        ns <= EVENT_BUDGET_NS * CI_BUDGET_MULTIPLIER,
        "event_emit median {ns} ns/call exceeds {} ns (2 x {EVENT_BUDGET_NS} ns budget)",
        EVENT_BUDGET_NS * CI_BUDGET_MULTIPLIER
    );
}

// ---------------------------------------------------------------------------
// Scenario 4: tool-registry lookup/dispatch
// ---------------------------------------------------------------------------

fn measure_tool_dispatch() {
    let ctx = PluginServices::new()
        .with_tools(Arc::new(PerfToolService::new()))
        .build();

    let callable = ctx.tools().expect("tool service").callable_tools();
    assert_eq!(callable.len(), 1, "the fake registry exposes one ToolInfo");
    assert_eq!(callable[0].name, "perf-noop");

    let ns = median_ns(
        "tool_dispatch (fake ToolService + real ToolRegistry lookup)",
        US_ITERS,
        || {
            ctx.tools().expect("tool service").execute_tool(
                "perf-call",
                "perf-noop",
                JsonValue::Null,
                ExecuteToolOptions::default(),
            )
        },
    );
    assert!(
        ns <= TOOL_BUDGET_NS * CI_BUDGET_MULTIPLIER,
        "tool_dispatch median {ns} ns/call exceeds {} ns (2 x {TOOL_BUDGET_NS} ns budget)",
        TOOL_BUDGET_NS * CI_BUDGET_MULTIPLIER
    );
}

// ---------------------------------------------------------------------------
// Scenario 5: dialog/UI action enqueue
// ---------------------------------------------------------------------------

fn measure_dialog_enqueue() {
    let ctx = PluginServices::new()
        .with_ui(Arc::new(PerfUiService::default()))
        .build();
    let options = [SelectOption::text("alpha"), SelectOption::text("beta")];

    let ns = median_ns("dialog_enqueue (fake UiService::select)", US_ITERS, || {
        ctx.ui()
            .expect("ui service")
            .select("perf-dialog", &options, DialogOptions::default())
    });
    assert!(
        ns <= DIALOG_BUDGET_NS * CI_BUDGET_MULTIPLIER,
        "dialog_enqueue median {ns} ns/call exceeds {} ns (2 x {DIALOG_BUDGET_NS} ns budget)",
        DIALOG_BUDGET_NS * CI_BUDGET_MULTIPLIER
    );
}

// ---------------------------------------------------------------------------
// Scenario 6: renderer/factory cache hit
// ---------------------------------------------------------------------------

fn measure_renderer_cache() {
    let renderers = Arc::new(PerfRendererService::default());
    renderers.register_message("perf-message", Arc::new(NoopRenderer));
    let ctx = PluginServices::new()
        .with_renderers(renderers as Arc<dyn RendererService>)
        .build();

    assert!(
        ctx.renderers()
            .expect("renderer service")
            .has(RendererKind::Message, "perf-message"),
        "the perf renderer must be registered"
    );

    let ns = median_ns(
        "renderer_cache (fake RendererService::has)",
        US_ITERS,
        || {
            ctx.renderers()
                .expect("renderer service")
                .has(RendererKind::Message, "perf-message")
        },
    );
    assert!(
        ns <= RENDERER_BUDGET_NS * CI_BUDGET_MULTIPLIER,
        "renderer_cache median {ns} ns/call exceeds {} ns (2 x {RENDERER_BUDGET_NS} ns budget)",
        RENDERER_BUDGET_NS * CI_BUDGET_MULTIPLIER
    );
}

// ---------------------------------------------------------------------------
// Scenario 7: service path vs the direct core cell access
// ---------------------------------------------------------------------------

fn measure_direct_vs_service() {
    // -- entries ------------------------------------------------------------
    let fake = Arc::new(PerfEntriesSession::new(entries_payload()));
    let ctx = PluginServices::new()
        .with_session(fake.clone() as Arc<dyn SessionService>)
        .build();

    // The direct call is also an indirect (function-pointer) call, so the ratio
    // isolates the service accessor + vtable hop rather than inlining luck.
    let direct_entries: fn(&PerfEntriesSession) -> Option<Arc<JsonValue>> =
        PerfEntriesSession::direct_entries;
    let entries_direct_ns = median_ns(
        "entries_direct (fake concrete cell, direct core call)",
        NS_ITERS,
        || direct_entries(&fake),
    );
    let entries_service_ns = median_ns(
        "entries_service (PluginContext::session().entries)",
        NS_ITERS,
        || ctx.session().expect("session service").entries(),
    );
    let entries_ratio = entries_service_ns as f64 / entries_direct_ns.max(1) as f64;
    println!(
        "entries_service_vs_direct: {entries_ratio:.2}x \
         (service {entries_service_ns} ns, direct {entries_direct_ns} ns)"
    );
    // 1.05x plus a 3 ns measurement floor: at this scale one vtable load and
    // one integer-nanosecond quantum dominate the ratio.
    let allowed_ns = (entries_direct_ns as f64 * 1.05).ceil() as u128 + 3;
    assert!(
        entries_service_ns <= allowed_ns,
        "entries service path {entries_service_ns} ns exceeds 1.05x direct \
         ({entries_direct_ns} ns -> {allowed_ns} ns incl. 3 ns floor)"
    );

    // -- status -------------------------------------------------------------
    let status = Arc::new(PerfStatusService::default());
    let status_ctx = PluginServices::new()
        .with_status(status.clone() as Arc<dyn StatusService>)
        .build();

    let status_direct_set_ns = median_ns(
        "status_direct_set (fake concrete cell, direct core call)",
        NS_ITERS,
        || status.direct_set_status("perf.direct", Some("running")),
    );
    let status_service_set_ns = median_ns(
        "status_service_set (PluginContext::status().set_status)",
        NS_ITERS,
        || {
            status_ctx
                .status()
                .expect("status service")
                .set_status("perf.direct", Some("running"))
        },
    );
    let status_set_ratio = status_service_set_ns as f64 / status_direct_set_ns.max(1) as f64;
    println!(
        "status_set_service_vs_direct: {status_set_ratio:.2}x \
         (service {status_service_set_ns} ns, direct {status_direct_set_ns} ns; \
         trait-object overhead reported, not gated)"
    );

    let status_direct_get_ns = median_ns(
        "status_direct_get (fake concrete cell, direct core call)",
        NS_ITERS,
        || status.direct_status("perf.direct"),
    );
    let status_service_get_ns = median_ns(
        "status_service_get (PluginContext::status().status)",
        NS_ITERS,
        || {
            status_ctx
                .status()
                .expect("status service")
                .status("perf.direct")
        },
    );
    let status_get_ratio = status_service_get_ns as f64 / status_direct_get_ns.max(1) as f64;
    println!(
        "status_get_service_vs_direct: {status_get_ratio:.2}x \
         (service {status_service_get_ns} ns, direct {status_direct_get_ns} ns; \
         trait-object overhead reported, not gated)"
    );
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Run every measurement. The live fixtures (status/events) are built here too,
/// so the header states the backend split once.
fn run_all() {
    println!();
    println!("=== native plugin service performance (release build) ===");
    println!(
        "backends: status/events = live AgentSession-bound adapters; \
         entries/tools/dialog/renderers = hermetic fakes"
    );
    println!(
        "budgets: status {STATUS_BUDGET_NS} ns, entries {ENTRIES_BUDGET_NS} ns, \
         event {EVENT_BUDGET_NS} ns, tool {TOOL_BUDGET_NS} ns, \
         dialog {DIALOG_BUDGET_NS} ns, renderer {RENDERER_BUDGET_NS} ns; \
         assertions use {CI_BUDGET_MULTIPLIER}x"
    );
    measure_status_set_get();
    measure_entries_arc();
    measure_event_emit();
    measure_tool_dispatch();
    measure_dialog_enqueue();
    measure_renderer_cache();
    measure_direct_vs_service();
    println!("=== done ===");
}

#[test]
#[ignore = "perf: run in release"]
fn status_set_get() {
    measure_status_set_get();
}

#[test]
#[ignore = "perf: run in release"]
fn entries_arc() {
    measure_entries_arc();
}

#[test]
#[ignore = "perf: run in release"]
fn event_emit() {
    measure_event_emit();
}

#[test]
#[ignore = "perf: run in release"]
fn tool_dispatch() {
    measure_tool_dispatch();
}

#[test]
#[ignore = "perf: run in release"]
fn dialog_enqueue() {
    measure_dialog_enqueue();
}

#[test]
#[ignore = "perf: run in release"]
fn renderer_cache() {
    measure_renderer_cache();
}

#[test]
#[ignore = "perf: run in release"]
fn direct_vs_service() {
    measure_direct_vs_service();
}

#[test]
#[ignore = "perf: run in release"]
fn all_services() {
    run_all();
}
