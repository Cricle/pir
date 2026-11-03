//! SDK facade benchmarks: construction, prompt round-trip/event delivery,
//! transcript reads, and session attach.
//!
//! Everything here is offline and deterministic: the model is an in-process
//! [`Provider`] that answers every request with the same small text reply, and
//! session fixtures are generated once in a per-process temp directory through
//! the real [`SessionManager`] append path. No network, no `~/.pi` state, no
//! API keys.
//!
//! Run in either profile:
//!
//! ```text
//! cargo bench --bench sdk_benchmark
//! cargo bench --bench sdk_benchmark --profile dev
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, BatchSize, Criterion};
use futures_util::StreamExt;
use serde_json::json;

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, ModelCost,
    ModelCostRates, StopReason, StreamOptions, TerminalReason, TextContent, ThinkingLevel, Usage,
    UserMessage, UserRole,
};
use pir::sdk::{Agent, AgentBuilder, EventBus, SdkEvent, SessionEvent, SessionMode};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Deterministic in-process provider
// ---------------------------------------------------------------------------

const BENCH_REPLY: &str = "benchmark reply";
const FIXTURE_TURNS: usize = 200;

struct BenchProvider {
    calls: AtomicUsize,
}

impl BenchProvider {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

fn bench_model() -> Model {
    Model {
        id: "bench-model".into(),
        name: "Bench model".into(),
        api: "bench-api".into(),
        provider: "bench".into(),
        base_url: "http://127.0.0.1:1".into(),
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
        context_window: 32_768,
        max_tokens: 4_096,
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

fn text_block(text: impl Into<String>) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant_message(text: impl Into<String>) -> AssistantMessage {
    AssistantMessage {
        api: "bench-api".into(),
        provider: "bench".into(),
        model: "bench-model".into(),
        content: vec![text_block(text)],
        stop_reason: StopReason::Stop,
        usage: Usage {
            input: 10,
            output: 5,
            total_tokens: 15,
            ..Usage::default()
        },
        ..AssistantMessage::default()
    }
}

impl Provider for BenchProvider {
    fn id(&self) -> &str {
        "bench"
    }

    fn name(&self) -> &str {
        "bench provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![bench_model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        // Keyless: `with_registry` streams unchecked.
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: BENCH_REPLY.to_owned(),
            partial: Arc::new(assistant_message(BENCH_REPLY)),
        });
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            assistant_message(BENCH_REPLY),
        ));
        stream
    }
}

fn bench_registry() -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(BenchProvider::new()));
    Arc::new(registry)
}

// ---------------------------------------------------------------------------
// Session fixture (real SessionManager JSONL append path)
// ---------------------------------------------------------------------------

/// Per-process temp directory removed when the benchmark function returns.
struct BenchDir {
    path: PathBuf,
}

impl BenchDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("pir-sdk-bench-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create bench temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BenchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn user_message(turn: usize) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(format!(
            "benchmark turn {turn}: explain the transcript projection contract \
             without repeating yourself; this line pads the medium fixture so \
             reads and JSON serialization move a representative payload."
        ))],
        tools_added: None,
        tools_removed: None,
        timestamp: turn as u64,
        sections: None,
    })
}

/// Write a medium (2 x [`FIXTURE_TURNS`] messages, well under the paged-open
/// caps) session file through the in-tree manager, then close it.
fn create_session_fixture(dir: &Path) -> PathBuf {
    let session_dir = dir.join("fixture-sessions");
    let mut manager = SessionManager::create(&dir.to_string_lossy(), &session_dir)
        .expect("create fixture session");
    let path = manager.path().to_path_buf();
    for turn in 0..FIXTURE_TURNS {
        manager
            .append_message(user_message(turn))
            .expect("append fixture user message");
        manager
            .append_message(Message::Assistant(assistant_message(format!(
                "fixture assistant reply {turn}: deterministic benchmark text."
            ))))
            .expect("append fixture assistant message");
    }
    manager.sync_now().expect("sync fixture session");
    drop(manager);
    path
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime")
}

fn build_in_memory_agent(
    rt: &tokio::runtime::Runtime,
    dir: &Path,
    registry: &Arc<ProviderRegistry>,
) -> Agent {
    rt.block_on(
        AgentBuilder::new()
            .with_registry(Arc::clone(registry), bench_model())
            .agent_dir(dir)
            .no_session()
            .no_compaction()
            .thinking_level(ThinkingLevel::Off)
            .build(),
    )
    .expect("build in-memory agent")
}

fn build_persistent_agent(
    rt: &tokio::runtime::Runtime,
    dir: &Path,
    registry: &Arc<ProviderRegistry>,
) -> Agent {
    rt.block_on(
        AgentBuilder::new()
            .with_registry(Arc::clone(registry), bench_model())
            .agent_dir(dir)
            .session(SessionMode::Dir(dir.join("agent-sessions")))
            .no_compaction()
            .thinking_level(ThinkingLevel::Off)
            .build(),
    )
    .expect("build persistent agent")
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// `AgentBuilder::build` for the hermetic in-memory shape: model/registry
/// adoption, plugin host build, tool selection, and `Agent` assembly.
fn sdk_agent_build(c: &mut Criterion) {
    let dir = BenchDir::new("build");
    let rt = runtime();
    let registry = bench_registry();
    c.bench_function("sdk_agent_build_in_memory", |b| {
        b.iter(|| black_box(build_in_memory_agent(&rt, dir.path(), &registry)))
    });
}

/// A complete non-streaming prompt run against the in-process provider. This
/// is the path that must not project session/loop events into a stream that
/// does not exist.
fn sdk_prompt_roundtrip(c: &mut Criterion) {
    let dir = BenchDir::new("prompt");
    let rt = runtime();
    let registry = bench_registry();
    c.bench_function("sdk_prompt_roundtrip_small", |b| {
        b.iter_batched(
            || build_in_memory_agent(&rt, dir.path(), &registry),
            |agent| {
                black_box(
                    rt.block_on(agent.prompt("benchmark prompt"))
                        .expect("prompt"),
                );
            },
            BatchSize::PerIteration,
        )
    });
}

/// A complete streaming prompt run: loop events, session projections, the
/// bounded channel, and stream teardown for one small turn.
fn sdk_prompt_stream(c: &mut Criterion) {
    let dir = BenchDir::new("stream");
    let rt = runtime();
    let registry = bench_registry();
    c.bench_function("sdk_prompt_stream_small", |b| {
        b.iter_batched(
            || build_in_memory_agent(&rt, dir.path(), &registry),
            |agent| {
                let events: Vec<SdkEvent> =
                    rt.block_on(async { agent.prompt_stream("benchmark prompt").collect().await });
                black_box(events)
            },
            BatchSize::PerIteration,
        )
    });
}

/// `Agent::transcript` (typed messages) over the medium persisted fixture.
fn sdk_transcript_read(c: &mut Criterion) {
    let dir = BenchDir::new("transcript");
    let fixture = create_session_fixture(dir.path());
    let rt = runtime();
    let registry = bench_registry();
    let agent = rt
        .block_on(
            AgentBuilder::new()
                .with_registry(Arc::clone(&registry), bench_model())
                .agent_dir(dir.path())
                .session(SessionMode::Existing(fixture))
                .no_compaction()
                .thinking_level(ThinkingLevel::Off)
                .build(),
        )
        .expect("build transcript agent");

    c.bench_function("sdk_transcript_read_medium", |b| {
        b.iter(|| black_box(rt.block_on(agent.transcript())))
    });
}

/// `Agent::transcript_json` (upstream `AgentMessage` JSON) over the same
/// fixture — the conversion a host/RPC mirror pays per snapshot.
fn sdk_transcript_json(c: &mut Criterion) {
    let dir = BenchDir::new("transcript-json");
    let fixture = create_session_fixture(dir.path());
    let rt = runtime();
    let registry = bench_registry();
    let agent = rt
        .block_on(
            AgentBuilder::new()
                .with_registry(Arc::clone(&registry), bench_model())
                .agent_dir(dir.path())
                .session(SessionMode::Existing(fixture))
                .no_compaction()
                .thinking_level(ThinkingLevel::Off)
                .build(),
        )
        .expect("build transcript-json agent");

    c.bench_function("sdk_transcript_json_medium", |b| {
        b.iter(|| black_box(rt.block_on(agent.transcript_json())))
    });
}

/// `Agent::attach_session` over the medium fixture: tail-first open, recorded
/// model/thinking read, historical totals, and the storage swap.
fn sdk_session_attach(c: &mut Criterion) {
    let dir = BenchDir::new("attach");
    let fixture = create_session_fixture(dir.path());
    let rt = runtime();
    let registry = bench_registry();
    let agent = build_persistent_agent(&rt, dir.path(), &registry);

    c.bench_function("sdk_session_attach_medium", |b| {
        b.iter(|| {
            rt.block_on(agent.attach_session(&fixture))
                .expect("attach fixture session");
        })
    });
}

/// Native [`EventBus`] fan-out: one emit to eight subscribers, the typed
/// counterpart of upstream `createEventBus`.
fn sdk_event_bus_emit(c: &mut Criterion) {
    let bus = EventBus::<u64>::new();
    let mut subscriptions = Vec::new();
    for index in 0..8u64 {
        subscriptions.push(bus.on(move |value| {
            black_box(*value + index);
        }));
    }
    c.bench_function("sdk_event_bus_emit_8", |b| {
        b.iter(|| bus.emit(black_box(&42)));
    });
    drop(subscriptions);
}

/// `SdkEvent::from_session_event` for one `entry_appended` payload: the deep
/// clone a streaming consumer observes per transcript append.
fn sdk_event_projection(c: &mut Criterion) {
    let entry = json!({
        "id": "entry-1",
        "message": {
            "role": "assistant",
            "content": [{ "type": "text", "text": "benchmark event payload" }],
            "api": "bench-api",
            "provider": "bench",
            "model": "bench-model",
            "usage": { "input": 10, "output": 5, "totalTokens": 15 },
            "stopReason": "stop"
        }
    });
    let event = SessionEvent::EntryAppended { entry };
    c.bench_function("sdk_event_projection_entry_appended", |b| {
        b.iter(|| black_box(SdkEvent::from_session_event(black_box(&event))))
    });
}

/// Run the group on a dedicated 32 MiB-stack thread. `Criterion::block_on`
/// drives the real agent futures on the calling thread; the debug-profile
/// agent future is large enough to overflow the Windows main-thread stack, so
/// the whole benchmark body runs on an explicitly sized thread. The benchmark
/// itself measures only its closures, not this thread creation.
fn main() {
    std::thread::Builder::new()
        .name("sdk-bench".to_owned())
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let mut criterion = Criterion::default()
                .sample_size(20)
                .warm_up_time(Duration::from_secs(1))
                .measurement_time(Duration::from_secs(3))
                .configure_from_args();
            sdk_agent_build(&mut criterion);
            sdk_prompt_roundtrip(&mut criterion);
            sdk_prompt_stream(&mut criterion);
            sdk_transcript_read(&mut criterion);
            sdk_transcript_json(&mut criterion);
            sdk_session_attach(&mut criterion);
            sdk_event_bus_emit(&mut criterion);
            sdk_event_projection(&mut criterion);
            criterion.final_summary();
        })
        .expect("spawn SDK benchmark thread")
        .join()
        .expect("SDK benchmark thread panicked");
}
