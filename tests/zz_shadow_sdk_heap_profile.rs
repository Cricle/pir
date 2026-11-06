//! Heap-profiling harness for the SDK hot paths (lane S).
//!
//! Ignored by default: it installs the `dhat` global allocator for the whole
//! test binary and writes a DHAT JSON profile. Run ONE phase per invocation
//! (`PIR_HEAP_PHASE`), so each profile attributes exactly that phase:
//!
//! ```text
//! PIR_HEAP_PHASE=prompt cargo test --test sdk_heap_profile --locked -- --ignored --nocapture
//! ```
//!
//! Phases (fixtures mirror `benches/sdk_benchmark.rs`): `build`, `prompt`,
//! `stream`, `transcript`, `transcript_json`, `attach`, `projection`, `bus`.
//! Heavy phases are amplified N times inside one bracketed heap-snapshot
//! window (`PIR_HEAP_ITERS` overrides N) so their sites dominate the
//! cumulative profile; the printed delta is per-operation (divided by N).
//! The `prompt`/`stream` loops build a fresh agent per operation exactly like
//! the bench's `iter_batched` setup, so subtract the separately measured
//! `build` delta when reading the per-prompt numbers.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures_util::StreamExt as _;

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
// Deterministic in-process provider (byte-identical shape to the bench)
// ---------------------------------------------------------------------------

const BENCH_REPLY: &str = "benchmark reply";
const FIXTURE_TURNS: usize = 200;

struct BenchProvider {
    calls: AtomicUsize,
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

// ---------------------------------------------------------------------------
// Fixtures (same shapes as benches/sdk_benchmark.rs)
// ---------------------------------------------------------------------------

struct BenchDir {
    path: PathBuf,
}

impl BenchDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("pir-sdk-heap-{label}-{}", std::process::id()));
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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime")
}

fn bench_registry() -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(BenchProvider {
        calls: AtomicUsize::new(0),
    }));
    Arc::new(registry)
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

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn print_stats(label: &str, before: &dhat::HeapStats, after: &dhat::HeapStats, ops: u64) {
    let live = after.curr_bytes as i128 - before.curr_bytes as i128;
    let live_blocks = after.curr_blocks as i128 - before.curr_blocks as i128;
    let total = after.total_bytes as i128 - before.total_bytes as i128;
    let total_blocks = after.total_blocks as i128 - before.total_blocks as i128;
    println!(
        "[dhat] {label}: per-op live_bytes={}/op live_blocks={}/op total_bytes={}/op \
         total_blocks={}/op (ops={ops})",
        live / ops as i128,
        live_blocks / ops as i128,
        total / ops as i128,
        total_blocks / ops as i128,
    );
    println!(
        "[dhat] {label} window totals: live={live} live_blocks={live_blocks} \
         total={total} total_blocks={total_blocks}"
    );
}

fn phase() -> String {
    std::env::var("PIR_HEAP_PHASE").unwrap_or_else(|_| "build".to_owned())
}

fn iters() -> u64 {
    std::env::var("PIR_HEAP_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[test]
#[ignore = "heap profiling harness; run explicitly with PIR_HEAP_PHASE=<phase>"]
fn sdk_heap_profile() {
    let label = phase();
    let profile_path =
        std::env::temp_dir().join(format!("pir-sdk-heap-{label}-{}.json", std::process::id()));
    let profiler = dhat::Profiler::builder().file_name(&profile_path).build();
    println!("[dhat] phase: {label}");
    println!("[dhat] profile: {}", profile_path.display());
    println!(
        "[dhat] build: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );

    let dir = BenchDir::new(&label);
    let rt = runtime();
    let registry = bench_registry();
    // Amplification per phase: heavy multi-millisecond phases run a few
    // times, the sub-microsecond phases run thousands. `PIR_HEAP_ITERS`
    // overrides the default count.
    let default_iters = match label.as_str() {
        "build" => 8,
        "prompt" | "stream" => 8,
        "transcript" | "transcript_json" | "attach" => 16,
        "projection" => 2048,
        "bus" => 65_536,
        _ => 1,
    };
    let ops = match iters() {
        0 => default_iters,
        overridden => overridden,
    };

    match label.as_str() {
        "build" => {
            let before = dhat::HeapStats::get();
            for _ in 0..ops {
                let agent = build_in_memory_agent(&rt, dir.path(), &registry);
                std::hint::black_box(&agent);
                drop(agent);
            }
            let after = dhat::HeapStats::get();
            print_stats("build", &before, &after, ops);
        }
        "prompt" | "stream" => {
            // Fresh agent per operation, exactly like the bench's
            // `iter_batched` setup; the `build` phase measures the same loop
            // minus the prompt, so (prompt - build) / ops is the per-prompt
            // cost.
            let before = dhat::HeapStats::get();
            for _ in 0..ops {
                let agent = build_in_memory_agent(&rt, dir.path(), &registry);
                if label == "prompt" {
                    let outcome = rt
                        .block_on(agent.prompt("benchmark prompt"))
                        .expect("prompt");
                    std::hint::black_box(&outcome);
                } else {
                    let events: Vec<SdkEvent> = rt.block_on(async {
                        agent.prompt_stream("benchmark prompt").collect().await
                    });
                    std::hint::black_box(&events);
                }
                drop(agent);
            }
            let after = dhat::HeapStats::get();
            print_stats(&label, &before, &after, ops);
        }
        "transcript" | "transcript_json" => {
            let fixture = create_session_fixture(dir.path());
            let agent = rt
                .block_on(
                    AgentBuilder::new()
                        .with_registry(Arc::clone(&registry), bench_model())
                        .agent_dir(dir.path())
                        .session(SessionMode::Existing(fixture.clone()))
                        .no_compaction()
                        .thinking_level(ThinkingLevel::Off)
                        .build(),
                )
                .expect("build transcript agent");
            let before = dhat::HeapStats::get();
            for _ in 0..ops {
                if label == "transcript" {
                    let messages = rt.block_on(agent.transcript());
                    std::hint::black_box(&messages);
                } else {
                    let json = rt.block_on(agent.transcript_json());
                    std::hint::black_box(&json);
                }
            }
            let after = dhat::HeapStats::get();
            print_stats(&label, &before, &after, ops);
        }
        "attach" => {
            let fixture = create_session_fixture(dir.path());
            let agent = rt
                .block_on(
                    AgentBuilder::new()
                        .with_registry(Arc::clone(&registry), bench_model())
                        .agent_dir(dir.path())
                        .session(SessionMode::Dir(dir.path().join("agent-sessions")))
                        .no_compaction()
                        .thinking_level(ThinkingLevel::Off)
                        .build(),
                )
                .expect("build attach agent");
            let before = dhat::HeapStats::get();
            for _ in 0..ops {
                rt.block_on(agent.attach_session(&fixture)).expect("attach");
            }
            let after = dhat::HeapStats::get();
            print_stats("attach", &before, &after, ops);
        }
        "projection" => {
            let entry = serde_json::json!({
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
            let before = dhat::HeapStats::get();
            for _ in 0..ops {
                let projected = SdkEvent::from_session_event(&event);
                std::hint::black_box(&projected);
            }
            let after = dhat::HeapStats::get();
            print_stats("projection", &before, &after, ops);
        }
        "bus" => {
            let bus: EventBus<u64> = EventBus::new();
            let mut subscriptions = Vec::new();
            for index in 0..8u64 {
                subscriptions.push(bus.on(move |value| {
                    std::hint::black_box(*value + index);
                }));
            }
            let before = dhat::HeapStats::get();
            for _ in 0..ops {
                bus.emit(std::hint::black_box(&42));
            }
            let after = dhat::HeapStats::get();
            print_stats("bus", &before, &after, ops);
            drop(subscriptions);
        }
        other => {
            let _ = &label;
            panic!("unknown PIR_HEAP_PHASE `{other}`");
        }
    }

    // Keep the profiler alive long enough that the end-of-run snapshot is
    // dominated by the measured phase, then flush.
    drop(profiler);
    let profile_len = std::fs::metadata(&profile_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    println!(
        "[dhat] profile written: {} ({profile_len} bytes)",
        profile_path.display()
    );
    assert!(
        profile_path.exists(),
        "dhat profile was not written to {}",
        profile_path.display()
    );
}
