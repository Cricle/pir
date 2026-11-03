use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use pir::agent::tool_batch::{AgentToolCall, ToolResult};
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::ToolScheduler;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::types::{
    AssistantMessage, AssistantRole, ContentBlock, Message, Model, ModelCost, ModelCostRates,
    StopReason, TextContent, Usage, UserMessage, UserRole,
};
use pir::coding_agent::tools::{register_core_tools, resolve_shell_config, ShellConfig};
use pir::durable::harness::{Harness, HarnessOptions};
use pir::durable::{
    Context as DurableContext, ConversationRecord, EntryId, EntryRecord, JsonlStorage, Storage,
    StorageWrite, TaskId, TaskRecord, TaskState, ROOT_CONVERSATION_ID,
};
use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use pir::protocol::{
    decode_value, encode_cbor, encode_frame, encode_server_envelope, CborOptions, FrameDecoder,
    FrameOptions, ServerEnvelope, ServiceUpdateFrame,
};
use pir::tui::core::{Component, Rows, Terminal, TerminalSink, TuiCore};
use pir::tui::messages::{AssistantMessageComponent, ToolExecutionComponent};
use pir::tui::stdin_buffer::WriteLog;
use serde_json::json;

// ── Bench-only allocation probe (B3/B8 `peak_bytes`) ────────────────────────
//
// Criterion reports wall time only. The memory acceptance targets need a
// process-level heap counter, so this bench binary owns a pass-through
// `GlobalAlloc` that forwards every allocation to `System` and only counts
// inside an explicit [`AllocProbe`] section. When no probe is open the hot path
// is one relaxed atomic load, so the 23 existing benches keep their allocation
// shape and time baseline; the probe sections are always outside the criterion
// timed region and print a `[mem]` line the report can cite.
//
// `COUNTING` is thread-local so a probe only observes work on the bench thread
// (criterion runs the measured closure there). `MAX_ALLOCATION_BYTES` is
// process-global because `tokio::fs` performs the actual file reads on the
// blocking pool: a whole-file `Vec` there would otherwise be invisible. That
// global maximum is the strong shape signal for B3 recovery; `PEAK_BYTES` is a
// per-thread live high-water mark and may under-count blocking-pool buffers.
struct BenchCountingAllocator;

thread_local! {
    static ALLOC_COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOC_EVENTS: Cell<u64> = const { Cell::new(0) };
    static ALLOC_TOTAL_BYTES: Cell<u64> = const { Cell::new(0) };
    static ALLOC_LIVE_BYTES: Cell<u64> = const { Cell::new(0) };
    static ALLOC_PEAK_BYTES: Cell<u64> = const { Cell::new(0) };
}

static ALLOC_MEASURING: AtomicBool = AtomicBool::new(false);
static ALLOC_MAX_SINGLE: AtomicU64 = AtomicU64::new(0);

#[expect(
    unsafe_code,
    reason = "bench-only GlobalAlloc wrapper: delegates straight to System and \
              adds per-thread counters; all TLS slots are const-initialized so \
              reading them cannot recurse into the allocator"
)]
unsafe impl GlobalAlloc for BenchCountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if ALLOC_MEASURING.load(Ordering::Relaxed) {
            ALLOC_MAX_SINGLE.fetch_max(layout.size() as u64, Ordering::Relaxed);
            let _ = ALLOC_COUNTING.try_with(|counting| {
                if counting.get() {
                    let _ = ALLOC_EVENTS.try_with(|events| events.set(events.get() + 1));
                    let _ = ALLOC_TOTAL_BYTES
                        .try_with(|bytes| bytes.set(bytes.get() + layout.size() as u64));
                    let _ = ALLOC_LIVE_BYTES.try_with(|live| {
                        let next = live.get().saturating_add(layout.size() as u64);
                        live.set(next);
                        let _ = ALLOC_PEAK_BYTES.try_with(|peak| peak.set(peak.get().max(next)));
                    });
                }
            });
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ALLOC_MEASURING.load(Ordering::Relaxed) {
            let _ = ALLOC_COUNTING.try_with(|counting| {
                if counting.get() {
                    let _ = ALLOC_LIVE_BYTES
                        .try_with(|live| live.set(live.get().saturating_sub(layout.size() as u64)));
                }
            });
        }
        System.dealloc(ptr, layout);
    }
}

#[global_allocator]
static BENCH_ALLOCATOR: BenchCountingAllocator = BenchCountingAllocator;

#[derive(Debug, Clone, Copy)]
struct AllocStats {
    events: u64,
    /// Cumulative bytes allocated on the bench thread while the section ran.
    /// Never decreased on free, so it is the copy-volume signal.
    total_bytes: u64,
    /// Highest thread-local live-byte total observed in the section.
    peak_bytes: u64,
    /// Largest single allocation requested by any thread in the section.
    max_allocation_bytes: u64,
}

/// RAII measurement section; dropping it disables counting even on panic.
struct AllocProbe;

impl AllocProbe {
    fn begin() -> Self {
        ALLOC_EVENTS.with(|events| events.set(0));
        ALLOC_TOTAL_BYTES.with(|bytes| bytes.set(0));
        ALLOC_LIVE_BYTES.with(|live| live.set(0));
        ALLOC_PEAK_BYTES.with(|peak| peak.set(0));
        ALLOC_MAX_SINGLE.store(0, Ordering::Relaxed);
        ALLOC_COUNTING.with(|counting| counting.set(true));
        ALLOC_MEASURING.store(true, Ordering::SeqCst);
        Self
    }

    fn finish(self) -> AllocStats {
        let stats = AllocStats {
            events: ALLOC_EVENTS.with(Cell::get),
            total_bytes: ALLOC_TOTAL_BYTES.with(Cell::get),
            peak_bytes: ALLOC_PEAK_BYTES.with(Cell::get),
            max_allocation_bytes: ALLOC_MAX_SINGLE.load(Ordering::Relaxed),
        };
        drop(self);
        stats
    }
}

impl Drop for AllocProbe {
    fn drop(&mut self) {
        ALLOC_COUNTING.with(|counting| counting.set(false));
        ALLOC_MEASURING.store(false, Ordering::SeqCst);
        ALLOC_MAX_SINGLE.store(0, Ordering::Relaxed);
    }
}

fn report_alloc_probe(bench: &str, stats: AllocStats) {
    eprintln!(
        "[mem] {bench} events={} total_bytes={} peak_bytes={} max_allocation_bytes={}",
        stats.events, stats.total_bytes, stats.peak_bytes, stats.max_allocation_bytes
    );
}

fn assistant(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        api: "bench".into(),
        provider: "bench".into(),
        model: "bench".into(),
        stop_reason: StopReason::Stop,
        usage: Usage::default(),
        ..Default::default()
    }
}

fn user_message(text: impl Into<String>, timestamp: u64) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp,
    })
}

/// A real CBOR encode/decode round trip on a representative protocol request.
fn protocol_cbor(c: &mut Criterion) {
    let request = json!({
        "type": "request",
        "id": "benchmark-1",
        "request": {
            "command": "prompt",
            "sessionId": "session-1",
            "text": "Summarize the current repository changes."
        }
    });
    c.bench_function("protocol_cbor_encode_decode", |b| {
        b.iter(|| {
            let encoded = encode_cbor(black_box(&request), CborOptions::default()).unwrap();
            black_box(decode_value(&encoded, CborOptions::default()).unwrap());
        })
    });
}

/// Decode a frame in transport-sized fragments, exercising the incremental path.
fn frame_decode(c: &mut Criterion) {
    let payload = vec![b'x'; 4096];
    let frame = encode_frame(&payload).unwrap();
    c.bench_function("frame_decode_fragmented", |b| {
        b.iter(|| {
            let mut decoder = FrameDecoder::new(FrameOptions::default()).unwrap();
            for chunk in frame.chunks(37) {
                let frames = decoder.push(black_box(chunk)).unwrap();
                black_box(frames);
            }
            decoder.end().unwrap();
        })
    });
}

struct BenchTool;
impl AgentTool for BenchTool {
    fn name(&self) -> &'static str {
        "bench"
    }
    fn description(&self) -> &'static str {
        "benchmark tool"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

fn scheduler(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(BenchTool));
    let scheduler = ToolScheduler::default();
    let context = ToolContext::new(".");
    c.bench_function("scheduler_execute_batch", |b| {
        b.iter(|| {
            let calls = vec![AgentToolCall {
                index: 0,
                call_id: "bench-call".into(),
                tool_name: "bench".into(),
                arguments: serde_json::Value::Object(Default::default()),
                arguments_error: None,
            }];
            black_box(runtime.block_on(scheduler.execute_batch(calls, &registry, &context, None)));
        })
    });
}

/// Exercise the same event lifecycle used by SSE adapters without network or
/// provider state, keeping the benchmark deterministic and repeatable.
fn sse_event_stream(c: &mut Criterion) {
    c.bench_function("sse_event_stream_lifecycle", |b| {
        b.iter(|| {
            let stream = AssistantMessageEventStream::new();
            let message = assistant(black_box("streamed delta"));
            stream.push(pir::ai::types::AssistantMessageEvent::Start {
                partial: Arc::new(message.clone()),
            });
            stream.push(pir::ai::types::AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: Arc::new(message.clone()),
            });
            stream.push(pir::ai::types::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "streamed delta".into(),
                partial: Arc::new(message.clone()),
            });
            stream.push(AssistantMessageEventStream::done_event(
                pir::ai::types::TerminalReason::Stop,
                message,
            ));
            black_box(stream.pending_count());
        })
    });
}

/// Fixed per-run harness costs, measurable offline without a model:
/// (1) the full system prompt build for a representative default run
/// (default preamble, four core tools, one project context file, one skill),
/// and (2) JSON event shaping (`coding_agent::json::to_json_event`, the
/// `--mode json` wire contract) over a representative assistant turn.
///
/// This isolates pir's fixed per-prompt overhead so the fixed-cost delta
/// against upstream `pi` is measurable without a model: pi performs the same
/// prompt-assembly + JSON-shaping work, but on top of Node startup, which
/// cannot be benched from Rust — only pir's fixed costs are measured here;
/// pi's startup cost must be captured separately (e.g. `pi --mode json -p`
/// wall time minus its first model token) and compared manually.
fn harness_fixed_costs(c: &mut Criterion) {
    use pir::agent::agent_loop::AgentEvent;
    use pir::ai::types::{Context, TerminalReason, ToolCall};
    use pir::coding_agent::json::to_json_event;
    use pir::skills::SkillInfo;
    use pir::system_prompt::{build_system_prompt, SystemPromptInput, ToolInfo};
    use std::path::PathBuf;

    let tools: Vec<ToolInfo> = ["read", "bash", "edit", "write"]
        .into_iter()
        .map(|name| ToolInfo::new(name, format!("{name} tool for the benchmark harness")))
        .collect();
    let hidden_tools: Vec<String> = Vec::new();
    let project_files = vec![(
        PathBuf::from("AGENTS.md"),
        "Keep changes small and verified.".to_string(),
    )];
    let skills = vec![SkillInfo {
        name: "bench".to_string(),
        description: "benchmark skill for the fixed-cost harness bench".to_string(),
        path: PathBuf::from("skills/bench/SKILL.md"),
        disable_model_invocation: false,
    }];
    let cwd = std::env::temp_dir();
    let input = SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &tools,
        hidden_tools: &hidden_tools,
        project_files: &project_files,
        skills: &skills,
        cwd: &cwd,
        // No package docs: the bench feeds the builder explicitly instead of
        // depending on the ambient `PI_PACKAGE_DIR`.
        package_dir: None,
        custom_sections: Vec::new(),
    };

    c.bench_function("system_prompt_build_fixed_run", |b| {
        b.iter(|| black_box(build_system_prompt(black_box(&input))))
    });

    // One representative streaming turn: start, text deltas, a tool call
    // (the wire shape gains id/toolName on toolcall_start), and the final
    // done event with its authoritative message.
    let partial = assistant("partial");
    let mut tool_call_message = partial.clone();
    tool_call_message
        .content
        .push(ContentBlock::ToolCall(ToolCall {
            id: "call-bench-1".to_string(),
            name: "edit".to_string(),
            arguments: [("path".to_string(), json!("fixture.rs"))]
                .into_iter()
                .collect(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        }));
    let turn_events: Vec<AgentEvent> = vec![
        AgentEvent::TurnStart {
            turn: 1,
            context: Arc::new(Context::default()),
        },
        AgentEvent::StreamEvent {
            turn: 1,
            event: pir::ai::types::AssistantMessageEvent::Start {
                partial: Arc::new(partial.clone()),
            },
        },
        AgentEvent::StreamEvent {
            turn: 1,
            event: pir::ai::types::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "Running the fix now.".to_string(),
                partial: Arc::new(partial.clone()),
            },
        },
        AgentEvent::StreamEvent {
            turn: 1,
            event: pir::ai::types::AssistantMessageEvent::ToolCallStart {
                content_index: 1,
                partial: Arc::new(tool_call_message),
            },
        },
        AgentEvent::StreamEvent {
            turn: 1,
            event: pir::ai::types::AssistantMessageEvent::Done {
                reason: TerminalReason::Stop,
                message: assistant("done"),
            },
        },
    ];

    c.bench_function("json_event_shaping_fixed_run", |b| {
        b.iter(|| {
            for event in &turn_events {
                black_box(to_json_event(black_box(event)));
            }
        })
    });
}

// ---------------------------------------------------------------------------
// Tool-call fixed costs (PERF-SCOPE P2, item 1)
//
// Convention for every bench below: one *fixed* call of the real path, no
// model, no network, command/input fixed. Setup (fixture creation, registry
// construction shared across iterations, a pooled shell's first spawn) is
// untimed; the timed region is the call itself, and — criterion's `iter`
// model — the drop of the call's returned `ToolResult` is inside it.
// ---------------------------------------------------------------------------

/// Self-cleaning fixture directory: created once per bench group, removed when
/// the benchmark function returns (also on panic, via `Drop`).
struct BenchFixture {
    dir: std::path::PathBuf,
}

impl BenchFixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pir-bench-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create bench fixture directory");
        Self { dir }
    }

    fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn file(&self, name: &str) -> std::path::PathBuf {
        self.dir.join(name)
    }

    fn write(&self, name: &str, contents: &str) -> std::path::PathBuf {
        let path = self.file(name);
        std::fs::write(&path, contents).expect("write bench fixture file");
        path
    }
}

impl Drop for BenchFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Deterministic text fixture: `lines` fixed-shape lines (55 bytes each).
fn text_fixture(lines: usize) -> String {
    (1..=lines)
        .map(|n| format!("line {n:04}: the quick brown fox jumps over the lazy dog\n"))
        .collect()
}

/// The registry a session builds for the core tools (`register_core_tools`),
/// i.e. the map the agent loop looks tools up in.
fn core_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
}

/// Session tool-registry construction fixed cost: eight `Arc<dyn AgentTool>`
/// registrations into the name map (the map `register_core_tools` builds at
/// session setup). Pure in-process work — no IO, no shell, no JS runtime.
fn tool_registry_build(c: &mut Criterion) {
    c.bench_function("tool_registry_build_fixed_run", |b| {
        b.iter(|| black_box(core_registry()))
    });
}

/// One typical dispatch: tool name → registry lookup → trait-object call. The
/// target is the same no-op `BenchTool` the scheduler bench uses, so the
/// measured quantity is the dispatch path alone (hash lookup + `Arc` clone +
/// vtable call + one boxed-future poll) with no tool-internal IO or
/// allocation — the fixed overhead every tool call pays on top of its work.
fn tool_dispatch(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut registry = core_registry();
    registry.register(Arc::new(BenchTool));
    let context = ToolContext::new(std::env::temp_dir());
    let arguments = serde_json::Value::Object(Default::default());
    c.bench_function("tool_dispatch_fixed_run", |b| {
        b.iter(|| {
            let tool = registry
                .get(black_box("bench"))
                .expect("bench tool registered");
            black_box(runtime.block_on(tool.run(black_box(&arguments), &context)));
        })
    });
}

/// One `read` tool call on a small text file (60 lines, ~3.2 KB) living inside
/// the call's own workspace root (the tool rejects paths outside `ctx.cwd`).
/// Fixed cost = path confinement + canonicalization + file read + lossy UTF-8
/// decode + line split/join + the `truncate_head` limit scan and copy +
/// `ToolResult` construction. The fixture is well under the 2000-line /
/// 50 KiB defaults, so no truncation footer fires — the footer branches are
/// not part of this number. Idempotent (nothing writes), so a plain `iter`
/// measures each call against the same bytes.
fn tool_read_file(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let fixture = BenchFixture::new("read");
    fixture.write("fixture.txt", &text_fixture(60));
    let registry = core_registry();
    let tool = registry.get("read").expect("read tool registered");
    let context = ToolContext::new(fixture.dir());
    let arguments = serde_json::json!({ "path": "fixture.txt" });
    // Fail loudly at setup rather than silently benching the tool's error path.
    let probe = runtime.block_on(tool.run(&arguments, &context));
    assert!(
        !probe.is_error,
        "read bench fixture must be readable: {}",
        probe.content
    );
    c.bench_function("tool_read_file_fixed_run", |b| {
        b.iter(|| black_box(runtime.block_on(tool.run(black_box(&arguments), &context))))
    });
}

/// One `edit` tool call: replace a unique line in the same 60-line text file.
/// Fixed cost = `edits[]` parse + oldText match/apply + read + write of the
/// file + diff/detail shaping. The call dirties the fixture, so the file is
/// rewritten before every call by the (untimed) setup of `iter_batched_ref`.
/// `BatchSize::PerIteration` is load-bearing: one input per iteration, so every
/// timed call starts from the unedited file — a multi-iteration batch (e.g.
/// `SmallInput`) resets the fixture once and only its first call would apply an
/// edit, the rest landing on the far cheaper "oldText not found" error path.
/// The timed region is the call alone; its `ToolResult` is dropped outside the
/// measurement — the cleanest read of the mutation tool's own cost.
fn tool_edit_file(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let fixture = BenchFixture::new("edit");
    let original = text_fixture(60);
    fixture.write("fixture.txt", &original);
    let registry = core_registry();
    let tool = registry.get("edit").expect("edit tool registered");
    let context = ToolContext::new(fixture.dir());
    let arguments = serde_json::json!({
        "path": "fixture.txt",
        "edits": [{
            "oldText": "line 0030: the quick brown fox jumps over the lazy dog",
            "newText": "line 0030: rewritten by the benchmark",
        }],
    });
    let probe = runtime.block_on(tool.run(&arguments, &context));
    assert!(
        !probe.is_error,
        "edit bench fixture must apply: {}",
        probe.content
    );
    let fixture_path = fixture.file("fixture.txt");
    let reset = original;
    c.bench_function("tool_edit_file_fixed_run", |b| {
        b.iter_batched_ref(
            || std::fs::write(&fixture_path, &reset).expect("reset edit fixture"),
            |_| runtime.block_on(tool.run(black_box(&arguments), &context)),
            BatchSize::PerIteration,
        )
    });
}

/// The bash executable the bash tool would resolve (settings `shellPath`, then
/// Git Bash / PATH). `None` means this machine has no shell: the bash benches
/// then skip with a printed note instead of measuring the tool's
/// "No bash shell found" error path.
fn bench_bash_shell() -> Option<ShellConfig> {
    match resolve_shell_config(None) {
        Ok(shell) => Some(shell),
        Err(error) => {
            eprintln!("[bench] skipping bash tool benches: {error}");
            None
        }
    }
}

/// One spawn of `echo bench` through `shell`, mirroring the bash tool's
/// per-call spawn shape (bash.rs:925-966): `-c <command>` with a null stdin
/// for every shell, except legacy WSL bash — `bash -s` reads the command from
/// stdin, so passing it as an argument *and* a null stdin would silently run
/// an empty script. That arm writes `echo bench` to a piped stdin and closes
/// it, exactly like the tool does.
async fn spawn_bench_shell(shell: &ShellConfig, cwd: &std::path::Path) -> std::process::Output {
    let mut command = tokio::process::Command::new(shell.program());
    command
        .args(shell.args())
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if shell.stdin_transport() {
        command.stdin(Stdio::piped());
    } else {
        command.arg("echo bench");
        // Upstream stdio shape: never inherit the host's stdin.
        command.stdin(Stdio::null());
    }
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = command.spawn().expect("spawn bench shell");
    if shell.stdin_transport() {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().expect("bench shell stdin pipe");
        stdin
            .write_all(b"echo bench")
            .await
            .expect("write bench command to shell stdin");
        drop(stdin);
    }
    child.wait_with_output().await.expect("bench shell output")
}

/// Fresh-shell spawn fixed cost: process creation + one tiny `echo` + pipe
/// drain — the arm a call pays when the persistent shell pool is off (and the
/// one the pool's first call pays before the member is up). Deliberately not
/// routed through `BashTool`: the tool's pool switch is settings-driven with
/// no injection seam, and its post-spawn loop polls `try_wait` on a 50 ms tick
/// that would fold scheduling granularity into the number. So this bench
/// reports the process-creation *floor* of that arm, using the same program /
/// args / cwd / stdio shape as the tool, both shell arms included (see
/// `spawn_bench_shell`).
fn tool_bash_spawn(c: &mut Criterion) {
    let Some(shell) = bench_bash_shell() else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let fixture = BenchFixture::new("bash-spawn");
    // Setup probe (untimed): a shell whose `echo bench` produces no output —
    // e.g. legacy WSL `bash -s` fed the wrong transport — must fail loudly
    // here rather than silently bench an empty run.
    let probe = runtime.block_on(spawn_bench_shell(&shell, fixture.dir()));
    assert!(
        probe.status.success() && String::from_utf8_lossy(&probe.stdout).contains("bench"),
        "bash spawn bench probe must echo: status {:?}, stdout {:?}, stderr {:?}",
        probe.status,
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr),
    );
    c.bench_function("tool_bash_spawn_fixed_run", |b| {
        b.iter(|| {
            black_box(runtime.block_on(async {
                let output = spawn_bench_shell(&shell, fixture.dir()).await;
                black_box(output.stdout.len())
            }))
        })
    });
}

/// One full `bash` tool call (`echo bench`) through the registry: dispatch
/// plus the tool's own fixed costs — settings read (`shellCommandPrefix` /
/// `shellPath` / `shellPool`), shell resolution, workspace check, capture
/// buffers and `ToolResult` shaping. On the default settings (`shellPool` on)
/// this is the steady-state *pooled* round trip; the persistent member's
/// spawn is paid once and absorbed by criterion's warmup. With
/// `shellPool: false` in the machine's settings the same bench instead
/// reports the per-call spawn arm (which includes the tool's 50 ms
/// `try_wait` poll tick — see `tool_bash_spawn_fixed_run` for that arm's
/// process-creation floor). Settings-dependent by construction: it is the
/// number a real session pays on this machine.
fn tool_bash_call(c: &mut Criterion) {
    if bench_bash_shell().is_none() {
        return;
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let fixture = BenchFixture::new("bash-call");
    let registry = core_registry();
    let tool = registry.get("bash").expect("bash tool registered");
    let context = ToolContext::new(fixture.dir());
    let arguments = serde_json::json!({ "command": "echo bench" });
    let probe = runtime.block_on(tool.run(&arguments, &context));
    assert!(
        !probe.is_error,
        "bash bench command must run: {}",
        probe.content
    );
    c.bench_function("tool_bash_call_fixed_run", |b| {
        b.iter(|| black_box(runtime.block_on(tool.run(black_box(&arguments), &context))))
    });
}

/// Inline JS extension for the bridge bench: registers one echo tool at
/// factory time (no `session_start` needed) whose `execute` returns a single
/// text block. Plain JSON schema (no TypeBox import) keeps the fixture's
/// surface minimal.
const BRIDGE_TOOL_TS: &str = r#"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function benchBridgeExtension(pi: ExtensionAPI) {
    pi.registerTool({
        name: "bench_bridge_echo",
        label: "Bench Bridge Echo",
        description: "Fixed-cost probe: echoes one string across the QuickJS bridge.",
        parameters: {
            type: "object",
            properties: { message: { type: "string", description: "Message to echo" } },
            required: ["message"],
        },
        async execute(_toolCallId, params) {
            return { content: [{ type: "text", text: `[bench] ${params.message}` }] };
        },
    });
}
"#;

/// One round trip across the Rust → QuickJS tool bridge:
/// `ExtensionRuntime::call_tool` = JSON-serialize the params, invoke
/// `__pir_callTool` (schema check + JS `execute`), JSON-serialize the result
/// back, parse it into the Rust `ToolResult`. The runtime, transpile and
/// extension load are setup (untimed, once); the timed region is the call
/// alone — no fixture file, no network, no model. Driven the way the
/// integration tests drive it (current-thread tokio runtime, awaiting
/// `call_tool` directly). A setup failure skips the bench with a printed note
/// rather than aborting the whole bench binary.
fn extension_tool_bridge(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fixture = BenchFixture::new("ext-bridge");
    let skip = |error: &dyn std::fmt::Display| {
        eprintln!("[bench] skipping extension_tool_bridge_fixed_run: {error}");
    };
    let Some(js_runtime) = runtime.block_on(async {
        let extension = match ExtensionRuntime::new(ExtensionConfig {
            cwd: fixture.dir().to_path_buf(),
            mode: "headless".to_string(),
            has_ui: false,
            model: Some("bench".to_string()),
            session_id: Some("bench-session".to_string()),
            system_prompt: "bench".to_string(),
            is_idle: true,
            // `None`: the runtime resolves the limits from the global settings
            // + `PI_EXT_*` env, like every host construction path.
            limits: None,
        })
        .await
        {
            Ok(extension) => extension,
            Err(error) => {
                skip(&format!("extension runtime construction failed: {error}"));
                return None;
            }
        };
        if let Err(error) = extension.seed_module("pir-ext:bench-bridge.ts", BRIDGE_TOOL_TS) {
            skip(&format!("seed failed: {error}"));
            return None;
        }
        if let Err(error) = extension.seeded_load("pir-ext:bench-bridge.ts").await {
            skip(&format!("load failed: {error}"));
            return None;
        }
        Some(extension)
    }) else {
        return;
    };
    let arguments = serde_json::json!({ "message": "bench" });
    let probe = match runtime.block_on(js_runtime.call_tool(
        "bench_bridge_echo",
        "bench-call-1",
        &arguments,
    )) {
        Ok(result) => result,
        Err(error) => {
            skip(&format!("probe call failed: {error}"));
            return;
        }
    };
    assert_eq!(
        probe.content.first().map(|item| item.text.as_str()),
        Some("[bench] bench")
    );
    c.bench_function("extension_tool_bridge_fixed_run", |b| {
        b.iter(|| {
            let result = runtime
                .block_on(js_runtime.call_tool(
                    black_box("bench_bridge_echo"),
                    black_box("bench-call-1"),
                    black_box(&arguments),
                ))
                .expect("bridge tool call");
            black_box(result.content.len())
        })
    });
}

/// A component whose per-frame render is a cache hit: the cached lines are
/// pushed straight into the frame's recycled row canvas (the
/// `AssistantMessageComponent` segment-cache shape).
struct CachedRows(Vec<String>);

impl Component for CachedRows {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.clone()
    }

    fn render_into(&self, _width: usize, out: &mut Rows<'_>) {
        for line in &self.0 {
            out.push(line);
        }
    }
}

/// TUI steady-frame assembly: 400 cached transcript rows rendered repeatedly
/// through [`TuiCore::render`] (the full frame path: canvas fill, snapshot,
/// diff scan) plus the canvas-only loop. On a steady frame the row canvas
/// copies zero cached bytes (`RowsWork::row_write_bytes == 0`); the bench
/// measures the residual O(visible frame) work.
fn tui_frame_assembly(c: &mut Criterion) {
    let lines: Vec<String> = (0..400)
        .map(|index| format!("message row {index:03} {}", "x".repeat(64)))
        .collect();
    let terminal = Terminal::with_sink_factory(
        || Box::new(std::io::sink()) as Box<dyn TerminalSink>,
        WriteLog::disabled(),
    );
    let mut core = TuiCore::new(terminal);
    core.terminal().set_size(120, 40);
    core.push_content(Box::new(CachedRows(lines.clone())));
    // Warm the pools: the measured frame is the steady state.
    core.render();
    core.render();
    c.bench_function("tui_steady_frame_assembly", |b| {
        b.iter(|| {
            core.render();
            black_box(core.last_frame_rows_work())
        })
    });

    let mut pool: Vec<String> = Vec::new();
    {
        let mut rows = Rows::new(&mut pool);
        rows.rewind();
        for line in &lines {
            rows.push(line);
        }
        rows.trim();
    }
    c.bench_function("tui_steady_row_canvas", |b| {
        b.iter(|| {
            let mut rows = Rows::new(&mut pool);
            rows.rewind();
            for line in &lines {
                rows.push(line);
            }
            rows.trim();
            black_box(rows.work())
        })
    });
    core.stop();
}

/// Server service-update fanout encoding. The reference loop encodes one
/// envelope per subscription with the pre-R5 shape (clone the update `Value`
/// into every `ServiceUpdateFrame`, then CBOR-encode it), so it prices the
/// per-subscription deep copy the forwarder now skips (the live path encodes
/// the update body once per broadcast). `server_fanout_update_body_encode`
/// is that shared one-time body encode.
fn server_fanout_encode(c: &mut Criterion) {
    let update = json!({
        "t": "state",
        "m": "snapshot",
        "as": "ops",
        "ops": [["r", {
            "id": "bench-session",
            "phase": "turn",
            "payload": "x".repeat(8192),
        }]],
    });
    let options = FrameOptions::default();
    c.bench_function("server_fanout_encode_8_subscribers_reference", |b| {
        b.iter(|| {
            for index in 0..8 {
                let envelope = ServerEnvelope::ServiceUpdate(ServiceUpdateFrame {
                    type_: "service_update".to_string(),
                    subscription_id: format!("sub-{index}"),
                    update: update.clone(),
                });
                black_box(encode_server_envelope(&envelope, options).unwrap());
            }
        })
    });
    let cbor_options = CborOptions::default();
    c.bench_function("server_fanout_update_body_encode", |b| {
        b.iter(|| black_box(encode_cbor(&update, cbor_options).unwrap()))
    });
}

/// Google SSE fixture (the same shape `tests/suites/ai_google.rs` parses):
/// thought/text deltas, a function call and a final usage/finish block.
const GOOGLE_SSE_FIXTURE: &str = r#"data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"Think"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":0,"thoughtsTokenCount":1,"totalTokenCount":11}}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"ing hard","thoughtSignature":"AAAAAAAAAAAAAAAAAAAAAA=="}]}}]}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"text":"Hello "}]}}]}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"text":"world","thoughtSignature":"BBBBBBBBBBBBBBBBBBBBAA=="}]}}]}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city":"Paris"}},"thoughtSignature":"CCCCCCCCCCCCCCCCCCCCAA=="}]}}]}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":9,"thoughtsTokenCount":3,"cachedContentTokenCount":2,"totalTokenCount":24}}

"#;

fn google_bench_model() -> Model {
    Model {
        id: "gemini-2.5-flash".to_string(),
        name: "gemini-2.5-flash".to_string(),
        api: "google-generative-ai".to_string(),
        provider: "google".to_string(),
        base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string(), "image".to_string()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 1.25,
                output: 10.0,
                cache_read: 0.31,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 1_048_576,
        max_tokens: 65_536,
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

/// Incremental Google SSE parsing over the fixed fixture above: the parser
/// state machine (`GoogleSseParser`), which every streamed Google turn pays
/// per provider chunk. The fixture and model are fixed; no network.
fn sse_google_parse(c: &mut Criterion) {
    let model = google_bench_model();
    let body = GOOGLE_SSE_FIXTURE.as_bytes();
    let probe = pir::ai::api::google::parse_google_sse(body, &model).expect("SSE fixture parses");
    assert!(!probe.is_empty(), "SSE fixture must produce events");
    c.bench_function("sse_google_parse_fixture", |b| {
        b.iter(|| {
            black_box(
                pir::ai::api::google::parse_google_sse(black_box(body), &model)
                    .expect("SSE fixture parses"),
            )
        })
    });
}

/// Durable session replay: open a 500-message JSONL session file from disk
/// (header + entries, pipeline of read → line split → parse → projection).
/// The file is written once during setup; the timed region is
/// `SessionManager::open` alone.
fn durable_session_replay(c: &mut Criterion) {
    use pir::session_manager::SessionManager;

    let fixture = BenchFixture::new("durable-replay");
    let mut manager =
        SessionManager::create_with_id(".", fixture.dir(), "bench-replay").expect("create session");
    for index in 0..500 {
        manager
            .append_message(Message::User(UserMessage {
                role: UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: format!(
                        "replay message {index:03}: the quick brown fox jumps over the lazy dog"
                    ),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                sections: None,
                timestamp: index as u64,
            }))
            .expect("append replay message");
    }
    let path = manager.path().to_path_buf();
    assert!(
        path.is_file(),
        "replay session file must exist after appends"
    );
    drop(manager);
    // Fail loudly at setup rather than benching a broken fixture.
    let probe = SessionManager::open(&path).expect("replay fixture opens");
    drop(probe);
    c.bench_function("durable_session_replay_500_entries", |b| {
        b.iter(|| black_box(SessionManager::open(black_box(&path)).expect("replay session")))
    });
}

/// Durable session append fixed cost (R1-L3-001 hot write path).
///
/// `append_message` is the write every turn pays: Serde serialization of the
/// message entry plus one append write to the already-open file handle. The
/// manager is created and opened ONCE in setup (with one seed message so the
/// lazy file creation is outside the timed region); each timed iteration then
/// clones the fixed message in `iter_batched` setup and appends it. The file
/// grows by one entry per iteration by design — this is the cost of an
/// already-open, long-lived session, not a fresh-manager measurement. The
/// bench also guards the R1-L3-001 fix (line+newline merged into one write)
/// against a fixed-cost regression.
fn durable_session_append(c: &mut Criterion) {
    use pir::session_manager::SessionManager;

    let fixture = BenchFixture::new("session-append");
    let mut manager = SessionManager::create_with_id(".", fixture.dir(), "bench-append")
        .expect("create append session");
    manager
        .append_message(user_message("append seed message", 0))
        .expect("seed append session file");
    assert!(
        manager.path().is_file(),
        "append fixture must exist before timing"
    );
    let message = user_message("fixed-cost append message", 1);
    c.bench_function("durable_session_append_fixed_run", |b| {
        b.iter_batched(
            || message.clone(),
            |message| {
                black_box(
                    manager
                        .append_message(message)
                        .expect("append user message"),
                )
            },
            BatchSize::SmallInput,
        )
    });
}

/// Long-line session open (R1-L3-016): `SessionManager::open` on a valid v3
/// JSONL file with one 64 MiB entry line. `load_entries_from_file` re-scans
/// `pending[line_start..]` from 0 on every 1 MiB read while no newline is
/// found, so a single long entry is O(L²) in the line length. Setup writes
/// header + two ordinary entries + the 64 MiB user message through the real
/// manager, then probes one successful open; the timed region is the open
/// alone (the returned manager is dropped by `iter_batched` outside timing).
fn session_open_long_line(c: &mut Criterion) {
    use pir::session_manager::SessionManager;

    let fixture = BenchFixture::new("session-long-line");
    let mut manager = SessionManager::create_with_id(".", fixture.dir(), "bench-long-line")
        .expect("create long-line session");
    for index in 0..2 {
        manager
            .append_message(user_message(format!("ordinary entry {index}"), index))
            .expect("append ordinary entry");
    }
    // One legal JSON string value: no newlines, so the entry stays a single
    // JSONL line. 64 MiB is below the 256 MiB MAX_SESSION_LINE_BYTES guard.
    let long_text = "x".repeat(64 * 1024 * 1024);
    manager
        .append_message(user_message(long_text, 2))
        .expect("append 64 MiB entry");
    let path = manager.path().to_path_buf();
    assert!(path.is_file(), "long-line fixture must exist");
    drop(manager);
    let probe = SessionManager::open(&path).expect("long-line fixture opens");
    drop(probe);
    {
        let alloc = AllocProbe::begin();
        let probe = SessionManager::open(&path).expect("long-line fixture opens");
        let stats = alloc.finish();
        eprintln!(
            "[mem] session_open_long_line_64mb events={} total_bytes={} peak_bytes={} max_allocation_bytes={}",
            stats.events, stats.total_bytes, stats.peak_bytes, stats.max_allocation_bytes
        );
        drop(probe);
    }
    c.bench_function("session_open_long_line_64mb", |b| {
        b.iter_batched(
            || (),
            |_| black_box(SessionManager::open(black_box(&path)).expect("open long-line session")),
            BatchSize::PerIteration,
        )
    });
}

/// Durable context read on a ~100 MiB session (R1-L3-002 + R1-L3-003).
///
/// The plan's B1 fixture: 4,000 entries of 25 KiB each. It uses
/// `JsonlStorage` over a `BenchFixture` temp directory (the plan names
/// `MemoryStorage`; JsonlStorage delegates all scans to MemoryStorage, so the
/// timed read path is identical, while the fixture is a real self-cleaning
/// session store). Setup opens the storage and harness, creates the root
/// conversation, commits the 4,000 entries in 500-entry chunks and is entirely
/// untimed. The timed call is `Conversation::context` — the public
/// `Harness::read_context` path: paginated `scan_entries` (256 per page,
/// O(n²) deep copies) followed by `derive_context` cloning the full active
/// transcript three to four times.
fn durable_context_read(c: &mut Criterion) {
    const ENTRY_COUNT: usize = 4_000;
    const ENTRY_TEXT_BYTES: usize = 25 * 1024;
    const COMMIT_CHUNK: usize = 500;

    let fixture = BenchFixture::new("durable-context-100mb");
    let runtime = tokio::runtime::Runtime::new().expect("durable bench runtime");
    let storage: Arc<dyn Storage> = Arc::new(
        runtime
            .block_on(JsonlStorage::open(fixture.dir()))
            .expect("open durable fixture storage"),
    );
    let harness = runtime
        .block_on(Harness::open(
            Arc::clone(&storage),
            HarnessOptions::default(),
            DurableContext::background(),
        ))
        .expect("open durable harness");
    let conversation = runtime
        .block_on(harness.root(DurableContext::background()))
        .expect("create root conversation");
    let conversation_id = conversation.id();
    // One parsed message value is cloned into every entry; building it once
    // keeps setup focused on the storage commit rather than serde cost.
    let message = serde_json::to_value(user_message("x".repeat(ENTRY_TEXT_BYTES), 0))
        .expect("serialize durable message");
    for chunk_start in (0..ENTRY_COUNT).step_by(COMMIT_CHUNK) {
        let chunk_end = (chunk_start + COMMIT_CHUNK).min(ENTRY_COUNT);
        let mut writes = Vec::with_capacity(chunk_end - chunk_start);
        for index in chunk_start..chunk_end {
            writes.push(StorageWrite::Entry {
                value: EntryRecord {
                    id: EntryId::from_raw(1_000_000 + index as i64),
                    conversation_id,
                    kind: "pi.user".to_owned(),
                    model: Some(vec![message.clone()]),
                    data: None,
                    head: None,
                    edits: None,
                    by_task_id: None,
                },
            });
        }
        runtime
            .block_on(storage.commit(writes))
            .expect("commit durable fixture entries");
    }
    let context = DurableContext::background();
    {
        let alloc = AllocProbe::begin();
        let probe = runtime
            .block_on(conversation.context(context.clone(), None))
            .expect("read durable context probe");
        let stats = alloc.finish();
        eprintln!(
            "[mem] durable_context_read_100mb events={} total_bytes={} peak_bytes={} max_allocation_bytes={}",
            stats.events, stats.total_bytes, stats.peak_bytes, stats.max_allocation_bytes
        );
        drop(probe);
    }
    c.bench_function("durable_context_read_100mb", |b| {
        b.iter_batched(
            || (),
            |_| {
                black_box(
                    runtime
                        .block_on(conversation.context(context.clone(), None))
                        .expect("read durable context"),
                )
            },
            BatchSize::PerIteration,
        )
    });
}

/// Fixture for the print widen-path memory bench: a valid v3 JSONL session
/// that is one linear parent chain of user/assistant message pairs (the shape
/// `SessionManager::open_paged` can stream without widening). Each message
/// carries `text_bytes` of ASCII text, so the file size is dominated by model
/// context rather than JSON bookkeeping.
fn write_linear_session_fixture(
    path: &std::path::Path,
    pair_count: usize,
    text_bytes: usize,
) -> u64 {
    use std::io::Write as _;

    let file = std::fs::File::create(path).expect("create linear session fixture");
    let mut out = std::io::BufWriter::new(file);
    let cwd = path
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_owned());
    let stamp = "2026-01-01T00:00:00.000Z";
    writeln!(
        out,
        "{}",
        json!({
            "type": "session",
            "version": 3,
            "id": "bench-linear-session",
            "timestamp": stamp,
            "cwd": cwd,
        })
    )
    .expect("write session header");
    writeln!(
        out,
        "{}",
        json!({
            "type": "model_change",
            "id": "m0",
            "parentId": null,
            "timestamp": stamp,
            "provider": "bench",
            "modelId": "bench-model",
        })
    )
    .expect("write model change");
    writeln!(
        out,
        "{}",
        json!({
            "type": "thinking_level_change",
            "id": "t0",
            "parentId": "m0",
            "timestamp": stamp,
            "thinkingLevel": "off",
        })
    )
    .expect("write thinking level");
    let text = "x".repeat(text_bytes);
    let mut parent = "t0".to_owned();
    for index in 0..pair_count {
        let user_id = format!("u{index:06}");
        let user = Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: format!("question {index}: {text}"),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp: 0,
        });
        writeln!(
            out,
            "{}",
            json!({
                "type": "message",
                "id": user_id,
                "parentId": parent,
                "timestamp": stamp,
                "message": user,
            })
        )
        .expect("write user message");
        let asst_id = format!("a{index:06}");
        writeln!(
            out,
            "{}",
            json!({
                "type": "message",
                "id": asst_id,
                "parentId": user_id,
                "timestamp": stamp,
                "message": Message::Assistant(assistant(&format!("answer {index}: {text}"))),
            })
        )
        .expect("write assistant message");
        parent = asst_id;
    }
    out.flush().expect("flush session fixture");
    std::fs::metadata(path).expect("fixture metadata").len()
}

/// The print widen path (100 MB session, compaction off): a paged linear
/// session's complete model context must not materialize the transcript twice.
///
/// The former path (`widen_to_full` + `build_session_projection`) holds the
/// parsed `Vec<Arc<FileEntry>>` AND the cloned `Vec<Message>` at once. The
/// linear paged builder streams the file and MOVES each stored message into
/// the aggregate, so its probe peak must stay below one-and-a-half times the
/// file size while the old path exceeds it. Both probes run over the same
/// fixture in the same process, so the assertion is an A/B on the real code
/// paths, not a size special case. The timed criterion function is the new
/// builder (the path print now runs for a linear paged session).
fn print_paged_prompt_context_100mb(c: &mut Criterion) {
    use pir::session_manager::SessionManager;

    const MESSAGE_PAIRS: usize = 1_000;
    const TEXT_BYTES: usize = 50 * 1024;
    const PAGE_BYTES: usize = 1024 * 1024;
    const PAGE_ENTRIES: usize = 245;

    let fixture = BenchFixture::new("print-paged-prompt-context-100mb");
    let path = fixture.file("session.jsonl");
    let file_bytes = write_linear_session_fixture(&path, MESSAGE_PAIRS, TEXT_BYTES);
    assert!(
        (95_000_000..=130_000_000).contains(&file_bytes),
        "the widen fixture must stay in the intended ~100 MB band, got {file_bytes} bytes"
    );

    let paged = SessionManager::open_paged(&path, PAGE_BYTES, PAGE_ENTRIES)
        .expect("open the widen fixture paged");
    assert!(paged.is_paged(), "the fixture must leave a skipped prefix");

    let probe = AllocProbe::begin();
    let context = paged
        .build_paged_prompt_context()
        .expect("stream the paged prompt context")
        .expect("the fixture is a linear session");
    let lean_stats = probe.finish();
    assert_eq!(context.messages.len(), MESSAGE_PAIRS * 2);
    drop(context);
    report_alloc_probe("print_paged_prompt_context_100mb", lean_stats);
    assert!(
        lean_stats.peak_bytes < file_bytes + file_bytes / 2,
        "one transcript copy must stay below 1.5x the session size: {} vs {file_bytes}",
        lean_stats.peak_bytes
    );
    assert!(
        lean_stats.max_allocation_bytes < 8 * 1024 * 1024,
        "no whole-file buffer may be allocated (largest {} bytes)",
        lean_stats.max_allocation_bytes
    );

    // A/B reference: the pre-change widen + projection over the same file.
    let mut widened = SessionManager::open_paged(&path, PAGE_BYTES, PAGE_ENTRIES)
        .expect("open the widen fixture paged for the reference");
    let old_probe = AllocProbe::begin();
    widened
        .widen_to_full()
        .expect("widen the reference session");
    let projection = widened
        .build_session_projection()
        .expect("project the widened reference session");
    let old_stats = old_probe.finish();
    assert_eq!(projection.messages.len(), MESSAGE_PAIRS * 2);
    drop(projection);
    drop(widened);
    report_alloc_probe("print_widen_projection_100mb", old_stats);
    assert!(
        lean_stats.peak_bytes + file_bytes / 4 < old_stats.peak_bytes,
        "the streaming builder must remove the second transcript copy \
         (lean {} vs widened {} bytes)",
        lean_stats.peak_bytes,
        old_stats.peak_bytes
    );

    c.bench_function("print_paged_prompt_context_100mb", |b| {
        b.iter(|| {
            black_box(
                paged
                    .build_paged_prompt_context()
                    .expect("stream paged prompt context")
                    .expect("linear session"),
            )
        })
    });
}

/// JSONL recovery open on a ~100 MiB session log (B3, R1-L3-008 + R1-L3-009).
///
/// Fixture (setup, untimed): a `BenchFixture` temp directory written through
/// the real `JsonlStorage` commit path — one conversation record, 4,000
/// 25 KiB entries committed in 500-entry chunks (the main log is ~100 MiB),
/// plus 200 live task sidecars with 50 complete replacement records each.
/// Each task-sidecar line is deliberately kept below the 32 KiB compaction
/// threshold so the fixture really contains 50 records per task.
///
/// Recovery reclaims a live task's older snapshots (and truncates confirmed
/// sidecar tails) on open, so the timed directory is restored from a pristine
/// sidecar snapshot before every iteration. The 100 MiB main log is not
/// rewritten: recovery only appends/replays it. The timed call is
/// `JsonlStorage::open(directory)`, i.e. both marker passes, sidecar replay,
/// in-memory `MemoryStorage` rebuild and reclamation planning. The returned
/// storage is dropped by `iter_batched` outside the timed region.
///
/// Correctness gates: `sidecar_record_clones()` must stay at zero for the
/// probe open and every timed open (R1-L3-008's one copy of each sidecar
/// record), and the fixture byte range asserts that the 100 MB/10 MB shapes
/// are the intended ones. A separate ~10 MB clean fixture (no task sidecars)
/// is the non-regression control for the 100 MB recovery band.
///
/// Memory: an [`AllocProbe`] wraps the probe open and prints `[mem]` with
/// per-thread `peak_bytes` plus the process-global largest single allocation.
/// The latter catches the old whole-file-read shape even though `tokio::fs`
/// runs the read on a blocking-pool thread.
fn jsonl_recovery_open(c: &mut Criterion) {
    run_jsonl_recovery_bench(
        c,
        "jsonl-recovery-100mb",
        "jsonl_recovery_open_100mb",
        JsonlRecoveryShape {
            entry_count: 4_000,
            entry_text_bytes: 25 * 1024,
            task_count: 200,
            task_state_changes: 50,
            min_main_bytes: 95_000_000,
            max_main_bytes: 130_000_000,
        },
    );
}

/// The 10 MB clean control for B3: same entry shape and marker/replay path as
/// the 100 MB fixture, but no task sidecars, so a recovery regression that
/// only shows up as fixed overhead (or a 10 MB slowdown hidden by the large
/// fixture) is visible against a stable small baseline.
fn jsonl_recovery_open_10mb(c: &mut Criterion) {
    run_jsonl_recovery_bench(
        c,
        "jsonl-recovery-10mb",
        "jsonl_recovery_open_10mb",
        JsonlRecoveryShape {
            entry_count: 400,
            entry_text_bytes: 25 * 1024,
            task_count: 0,
            task_state_changes: 0,
            min_main_bytes: 9_500_000,
            max_main_bytes: 12_500_000,
        },
    );
}

struct JsonlRecoveryShape {
    entry_count: usize,
    entry_text_bytes: usize,
    task_count: usize,
    task_state_changes: usize,
    min_main_bytes: u64,
    max_main_bytes: u64,
}

struct JsonlRecoveryFixture {
    dir: std::path::PathBuf,
    sidecar_names: Vec<String>,
    pristine: std::path::PathBuf,
    main_bytes: u64,
    sidecar_bytes: u64,
}

fn build_jsonl_recovery_fixture(
    fixture: &BenchFixture,
    runtime: &tokio::runtime::Runtime,
    shape: &JsonlRecoveryShape,
) -> JsonlRecoveryFixture {
    const COMMIT_CHUNK: usize = 500;
    const TASK_CHECKPOINT_PAD: usize = 256;

    let dir = fixture.dir().to_path_buf();
    let storage = runtime
        .block_on(JsonlStorage::open(&dir))
        .expect("open jsonl recovery fixture");
    runtime
        .block_on(storage.commit(vec![StorageWrite::Conversation {
            value: ConversationRecord {
                id: ROOT_CONVERSATION_ID,
                parent: None,
                owner: None,
            },
        }]))
        .expect("commit fixture conversation");

    let message = serde_json::to_value(user_message("x".repeat(shape.entry_text_bytes), 0))
        .expect("serialize");
    for chunk_start in (0..shape.entry_count).step_by(COMMIT_CHUNK) {
        let chunk_end = (chunk_start + COMMIT_CHUNK).min(shape.entry_count);
        let mut writes = Vec::with_capacity(chunk_end - chunk_start);
        for index in chunk_start..chunk_end {
            writes.push(StorageWrite::Entry {
                value: EntryRecord {
                    id: EntryId::from_raw(1_000_000 + index as i64),
                    conversation_id: ROOT_CONVERSATION_ID,
                    kind: "pi.user".to_owned(),
                    model: Some(vec![message.clone()]),
                    data: None,
                    head: None,
                    edits: None,
                    by_task_id: None,
                },
            });
        }
        runtime
            .block_on(storage.commit(writes))
            .expect("commit fixture entries");
    }

    // 200 live tasks, each rewritten 50 times. One commit carries all 200
    // task records, so every task sidecar gains one complete replacement
    // record per round without paying 10,000 separate commit/fsync costs.
    let task_ids: Vec<TaskId> = (0..shape.task_count)
        .map(|index| TaskId::from_raw(2_000_000 + index as i64))
        .collect();
    let checkpoint_pad = "p".repeat(TASK_CHECKPOINT_PAD);
    for step in 0..shape.task_state_changes {
        let mut writes = Vec::with_capacity(shape.task_count);
        for (index, task_id) in task_ids.iter().enumerate() {
            writes.push(StorageWrite::Task {
                value: TaskRecord {
                    id: *task_id,
                    conversation_id: ROOT_CONVERSATION_ID,
                    kind: "bench.task".to_owned(),
                    version: 1,
                    input: json!({ "task": index }),
                    owner: None,
                    background: true,
                    abort_requested: false,
                    abort_reason: None,
                    abandon_on_restart: None,
                    started_at: None,
                    ended_at: None,
                    restarts: None,
                    state: TaskState::Pending {
                        checkpoint: json!({ "step": step, "pad": &checkpoint_pad }),
                    },
                    memos: None,
                    trace: None,
                    links: Vec::new(),
                },
            });
        }
        runtime
            .block_on(storage.commit(writes))
            .expect("commit fixture task state");
    }
    runtime
        .block_on(storage.close())
        .expect("close jsonl recovery fixture");
    drop(storage);

    let main_path = fixture.file("main.jsonl");
    let main_bytes = std::fs::metadata(&main_path)
        .expect("main.jsonl exists")
        .len();
    assert!(
        (shape.min_main_bytes..=shape.max_main_bytes).contains(&main_bytes),
        "B3 main log must stay in the intended band ({}..={} bytes), got {main_bytes} bytes",
        shape.min_main_bytes,
        shape.max_main_bytes
    );

    let mut sidecar_names = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read jsonl fixture directory") {
        let entry = entry.expect("read jsonl fixture entry");
        if !entry
            .file_type()
            .expect("jsonl fixture file type")
            .is_file()
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("task-") && name.ends_with(".jsonl") {
            sidecar_names.push(name);
        }
    }
    sidecar_names.sort();
    assert_eq!(
        sidecar_names.len(),
        shape.task_count,
        "fixture must contain one sidecar per live task"
    );
    let mut sidecar_bytes = 0u64;
    for name in &sidecar_names {
        let path = dir.join(name);
        sidecar_bytes += std::fs::metadata(&path)
            .expect("task sidecar metadata")
            .len();
        let lines = std::fs::read_to_string(&path)
            .expect("read task sidecar")
            .lines()
            .count();
        assert_eq!(
            lines, shape.task_state_changes,
            "task sidecar {name} must retain all state changes"
        );
    }

    // Snapshot the sidecars before the probe: recovery truncates a live
    // task's older snapshots on the first open, and every timed iteration
    // must start from the same 50-record crash state.
    let pristine = dir.join("sidecar-pristine");
    std::fs::create_dir(&pristine).expect("create pristine sidecar directory");
    for name in &sidecar_names {
        std::fs::copy(dir.join(name), pristine.join(name)).expect("snapshot task sidecar");
    }

    JsonlRecoveryFixture {
        dir,
        sidecar_names,
        pristine,
        main_bytes,
        sidecar_bytes,
    }
}

fn restore_sidecars(fixture: &JsonlRecoveryFixture) {
    for name in &fixture.sidecar_names {
        std::fs::copy(fixture.pristine.join(name), fixture.dir.join(name))
            .expect("restore task sidecar before recovery");
    }
}

fn run_jsonl_recovery_bench(c: &mut Criterion, tag: &str, id: &str, shape: JsonlRecoveryShape) {
    use pir::durable::storage::jsonl::{reset_sidecar_record_clones, sidecar_record_clones};

    let fixture = BenchFixture::new(tag);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("jsonl recovery runtime");
    let fixture = build_jsonl_recovery_fixture(&fixture, &runtime, &shape);
    let dir = fixture.dir.clone();
    let sidecar_count = fixture.sidecar_names.len();
    let state_changes = shape.task_state_changes;

    // Peak probe: one untimed open on a pristine fixture, with the allocator
    // counter enabled. `max_allocation_bytes` is global (blocking-pool reads),
    // `peak_bytes` is this thread's live high-water mark.
    reset_sidecar_record_clones();
    let probe_started = std::time::Instant::now();
    let probe = AllocProbe::begin();
    let probe_storage = runtime
        .block_on(JsonlStorage::open(&dir))
        .expect("probe jsonl recovery");
    let alloc_stats = probe.finish();
    let probe_elapsed = probe_started.elapsed();
    let probe_clones = sidecar_record_clones();
    drop(probe_storage);
    assert_eq!(
        probe_clones, 0,
        "recovery must not deep-clone sidecar records (R1-L3-008)"
    );
    restore_sidecars(&fixture);

    eprintln!(
        "[bench] {id} fixture: main={main_bytes} B, {sidecar_count} task sidecars x \
         {state_changes} records = {sidecar_bytes} B, first-open probe={probe_elapsed:?}",
        main_bytes = fixture.main_bytes,
        sidecar_bytes = fixture.sidecar_bytes,
    );
    report_alloc_probe(id, alloc_stats);

    c.bench_function(id, |b| {
        b.iter_batched(
            || {
                restore_sidecars(&fixture);
                reset_sidecar_record_clones();
            },
            |_| {
                let storage = runtime
                    .block_on(JsonlStorage::open(&dir))
                    .expect("recover jsonl storage");
                assert_eq!(
                    sidecar_record_clones(),
                    0,
                    "timed recovery must keep one copy of every sidecar record (R1-L3-008)"
                );
                black_box(storage)
            },
            BatchSize::PerIteration,
        )
    });
}

/// Session-selector keystroke over 1,000 sessions with ~100 KiB search
/// corpora (B8, R1-L3-012 + R1-L3-013).
///
/// The 1,000 `SessionSummary` values are built once outside the timed region
/// (100 MB of literal `all_messages_text` in total; every corpus is distinct
/// and carries the needle at the END, so a match scans the full 100 KiB).
/// `session_search_keystroke_1k` is the warm path: the first
/// `match_session_search` round populates the thread-local normalized-corpus
/// and fingerprint caches, so the timed loop must be a pure cache hit (no
/// `to_lowercase`, no re-hash). The post-bench assertions check builds,
/// fingerprints and fingerprint bytes did not move.
///
/// `session_search_keystroke_1k_cold` is the first-keystroke path: every
/// iteration clears the thread-local caches in `iter_batched` setup, so the
/// timed region pays `to_lowercase`/normalization plus the 100 MB literal
/// fingerprint hash. That is the path R1-L3-012's ≥30% target is about.
///
/// The cold probe open is wrapped in [`AllocProbe`] and prints `[mem]`
/// `peak_bytes`; the fixture itself stays allocated outside the probe.
fn session_search_keystroke(c: &mut Criterion) {
    use pir::session_manager::search::{
        match_session_search, reset_session_search_corpus_builds, session_search_corpus_builds,
        session_search_corpus_cache_stats, session_search_corpus_fingerprint_bytes,
        session_search_corpus_fingerprints, SessionSearchQuery,
    };
    use pir::session_manager::SessionSummary;

    const SESSION_COUNT: usize = 1_000;
    const CORPUS_BYTES: usize = 100 * 1024;
    const NEEDLE: &str = "benchmarkneedle";

    let seed = "the quick brown fox jumps over the lazy dog ";
    let mut filler = seed.repeat((CORPUS_BYTES - 64) / seed.len() + 1);
    filler.truncate(CORPUS_BYTES - 64);
    let mut summaries = Vec::with_capacity(SESSION_COUNT);
    for index in 0..SESSION_COUNT {
        let mut all_messages_text = String::with_capacity(CORPUS_BYTES);
        all_messages_text.push_str(&filler);
        all_messages_text.push_str(&format!(" session {index:04} {NEEDLE}"));
        summaries.push(SessionSummary {
            path: std::path::PathBuf::from(format!("C:/bench/sessions/session-{index:04}.jsonl")),
            id: format!("bench-session-{index:04}"),
            cwd: "C:/bench".to_owned(),
            modified: std::time::SystemTime::UNIX_EPOCH,
            name: Some(format!("bench session {index:04}")),
            first_message: "benchmark fixture".to_owned(),
            all_messages_text,
            message_count: 1,
            parent_session_path: None,
        });
    }
    let query = SessionSearchQuery::parse(NEEDLE);
    assert_eq!(
        query.tokens.len(),
        1,
        "B8 measures a single-token keystroke"
    );

    reset_session_search_corpus_builds();
    let cold_started = std::time::Instant::now();
    let probe = AllocProbe::begin();
    let first_round_matches = summaries
        .iter()
        .filter(|summary| match_session_search(&summary.search_fields(), &query).is_some())
        .count();
    let cold_stats = probe.finish();
    let cold_elapsed = cold_started.elapsed();
    assert_eq!(
        first_round_matches, SESSION_COUNT,
        "every fixture corpus must contain the needle"
    );
    let builds_after_first = session_search_corpus_builds();
    assert_eq!(
        builds_after_first, SESSION_COUNT as u64,
        "the first keystroke builds exactly one normalized corpus per session"
    );
    let fingerprints_after_first = session_search_corpus_fingerprints();
    assert_eq!(
        fingerprints_after_first, SESSION_COUNT as u64,
        "the first keystroke fingerprints exactly one literal corpus per session"
    );
    let fingerprint_bytes_after_first = session_search_corpus_fingerprint_bytes();
    assert!(
        fingerprint_bytes_after_first >= (SESSION_COUNT * (CORPUS_BYTES - 128)) as u64,
        "the first keystroke must hash the whole 100 MB literal corpus, got \
         {fingerprint_bytes_after_first} bytes"
    );
    let stats = session_search_corpus_cache_stats();
    assert_eq!(
        stats.entries, SESSION_COUNT,
        "all 1,000 corpora must stay resident"
    );
    assert_eq!(stats.evictions, 0, "no corpus may be evicted");
    eprintln!(
        "[bench] session_search_keystroke_1k fixture: {SESSION_COUNT} sessions x \
         {CORPUS_BYTES} B corpus, cold first keystroke={cold_elapsed:?}, \
         fingerprints={fingerprints_after_first}, fingerprint_bytes={fingerprint_bytes_after_first}"
    );
    report_alloc_probe("session_search_keystroke_1k_cold", cold_stats);

    c.bench_function("session_search_keystroke_1k", |b| {
        b.iter(|| {
            let matched = summaries
                .iter()
                .filter(|summary| match_session_search(&summary.search_fields(), &query).is_some())
                .count();
            black_box(matched)
        })
    });

    assert_eq!(
        session_search_corpus_builds(),
        builds_after_first,
        "second-round keystroke must be a cache hit (no corpus rebuild)"
    );
    assert_eq!(
        session_search_corpus_fingerprints(),
        fingerprints_after_first,
        "second-round keystroke must not re-hash any corpus"
    );
    assert_eq!(
        session_search_corpus_fingerprint_bytes(),
        fingerprint_bytes_after_first,
        "second-round keystroke must hash zero corpus bytes"
    );

    c.bench_function("session_search_keystroke_1k_cold", |b| {
        b.iter_batched(
            reset_session_search_corpus_builds,
            |_| {
                let matched = summaries
                    .iter()
                    .filter(|summary| {
                        match_session_search(&summary.search_fields(), &query).is_some()
                    })
                    .count();
                black_box(matched)
            },
            BatchSize::PerIteration,
        )
    });
    assert_eq!(
        session_search_corpus_builds(),
        SESSION_COUNT as u64,
        "every cold iteration must rebuild exactly one corpus per session"
    );

    // Drop the 100 MB thread-local corpus cache before the next fixture runs;
    // otherwise it would inflate every later bench's process RSS.
    reset_session_search_corpus_builds();
}

/// Real-directory session listing (B8, R1-L3-013 + R1-L3-017).
///
/// Two fixtures: 100 and 1,000 small valid v3 session files, each directory
/// also containing the same 256 MiB single-line outlier (a hard link to one
/// created file). The listing must return lazy corpus descriptors — no
/// message text is materialized (`session_listing_corpus_bytes() == 0`) — and
/// one file read per candidate, including the outlier. The timed call is
/// `SessionManager::list_sessions(dir, None)`.
///
/// The `[mem]` probe on each fixture records `peak_bytes`; both contain the
/// same outlier, so `1k <= 2 * 100` is a valid "peak does not grow linearly
/// with session count" comparison rather than an outlier-size comparison.
fn session_search_listing(c: &mut Criterion) {
    use pir::session_manager::{
        reset_session_list_file_reads, reset_session_listing_corpus_bytes, session_list_file_reads,
        session_listing_corpus_bytes, FileEntry, MessageEntry, SessionHeader, SessionManager,
        StoredMessage,
    };
    use std::io::Write as _;

    const SMALL_COUNT: usize = 100;
    const LARGE_COUNT: usize = 1_000;
    const OUTLIER_BYTES: usize = 256 * 1024 * 1024;
    const TIMESTAMP: &str = "2026-01-01T00:00:00.000Z";

    fn write_small_session(dir: &std::path::Path, index: usize) {
        let header = FileEntry::Session(SessionHeader::new(
            format!("bench-listing-{index:04}"),
            "C:/bench",
            TIMESTAMP.to_owned(),
        ));
        let message = FileEntry::Message(MessageEntry {
            id: format!("m-{index:04}"),
            parent_id: None,
            timestamp: TIMESTAMP.to_owned(),
            message: StoredMessage::from(user_message(
                format!("bench listing session {index:04} needle"),
                index as u64,
            )),
        });
        let mut text = serde_json::to_string(&header).expect("serialize listing header");
        text.push('\n');
        text.push_str(&serde_json::to_string(&message).expect("serialize listing message"));
        text.push('\n');
        std::fs::write(dir.join(format!("session-{index:04}.jsonl")), text)
            .expect("write listing session");
    }

    fn write_outlier(dir: &std::path::Path, payload_bytes: usize, id: &str) -> std::path::PathBuf {
        let path = dir.join("session-outlier.jsonl");
        let header = FileEntry::Session(SessionHeader::new(id, "C:/bench", TIMESTAMP.to_owned()));
        let header_line = serde_json::to_string(&header).expect("serialize outlier header");
        let file = std::fs::File::create(&path).expect("create outlier session");
        let mut writer = std::io::BufWriter::new(file);
        writeln!(writer, "{header_line}").expect("write outlier header");
        write!(
            writer,
            "{{\"type\":\"message\",\"id\":\"m-outlier\",\"parentId\":null,\
             \"timestamp\":\"{TIMESTAMP}\",\"message\":{{\"role\":\"user\",\"content\":\
             [{{\"type\":\"text\",\"text\":\""
        )
        .expect("write outlier prefix");
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..(payload_bytes / chunk.len()) {
            writer.write_all(&chunk).expect("write outlier payload");
        }
        writer.write_all(b"\"}]}}\n").expect("write outlier suffix");
        writer.flush().expect("flush outlier session");
        path
    }

    let small = BenchFixture::new("session-listing-100");
    for index in 0..SMALL_COUNT {
        write_small_session(small.dir(), index);
    }
    let outlier_100 = write_outlier(small.dir(), OUTLIER_BYTES, "bench-listing-outlier-100");

    let large = BenchFixture::new("session-listing-1k");
    for index in 0..LARGE_COUNT {
        write_small_session(large.dir(), index);
    }
    let outlier_1k = large.file("session-outlier.jsonl");
    if std::fs::hard_link(&outlier_100, &outlier_1k).is_err() {
        // Hard links can fail on exotic temp filesystems; a copy keeps the
        // fixture shape identical at the cost of one extra 256 MiB file.
        std::fs::copy(&outlier_100, &outlier_1k).expect("copy outlier into 1k listing fixture");
    }

    fn probe_listing(bench: &str, dir: &std::path::Path, expected: usize) -> (AllocStats, usize) {
        reset_session_listing_corpus_bytes();
        reset_session_list_file_reads();
        let probe = AllocProbe::begin();
        let sessions = SessionManager::list_sessions(dir, None);
        let stats = probe.finish();
        assert_eq!(
            sessions.len(),
            expected,
            "{bench} must list every session file"
        );
        assert_eq!(
            session_list_file_reads(),
            expected as u64,
            "{bench} must open each candidate exactly once"
        );
        assert_eq!(
            session_listing_corpus_bytes(),
            0,
            "{bench} listing must keep lazy corpus descriptors, not message text"
        );
        for summary in &sessions {
            assert!(
                summary.all_messages_text.len() < 1024,
                "{bench} returned an eager corpus ({} bytes), not a lazy descriptor",
                summary.all_messages_text.len()
            );
        }
        report_alloc_probe(bench, stats);
        (stats, sessions.len())
    }

    let (small_stats, small_listed) =
        probe_listing("session_search_listing_100", small.dir(), SMALL_COUNT + 1);
    let (large_stats, large_listed) = probe_listing(
        "session_search_listing_1k_outlier",
        large.dir(),
        LARGE_COUNT + 1,
    );
    assert!(
        large_stats.peak_bytes <= small_stats.peak_bytes.saturating_mul(2),
        "listing peak must not grow linearly with session count: 1k={} bytes vs 100={} bytes",
        large_stats.peak_bytes,
        small_stats.peak_bytes
    );
    // The fixture's 256 MiB single-line outlier must be scanned, not
    // buffered: before the listing peek capped its line buffer, both probes
    // peaked at ~800 MB (a 512 MB single allocation from the growing `Vec`).
    // 64 MiB leaves room for allocator slack while still failing loudly on a
    // return to whole-line buffering.
    const LISTING_PEAK_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
    for (bench, stats) in [
        ("session_search_listing_100", small_stats),
        ("session_search_listing_1k_outlier", large_stats),
    ] {
        assert!(
            stats.peak_bytes < LISTING_PEAK_LIMIT_BYTES,
            "{bench} listing must not buffer the 256 MiB outlier: peak={} bytes",
            stats.peak_bytes
        );
    }
    eprintln!(
        "[bench] session_search_listing fixture: 100 sessions + 256 MiB outlier listed {}, \
         1,000 sessions + outlier listed {}",
        small_listed, large_listed
    );

    let small_dir = small.dir().to_path_buf();
    c.bench_function("session_search_listing_100", |b| {
        b.iter(|| black_box(SessionManager::list_sessions(&small_dir, None)))
    });
    let large_dir = large.dir().to_path_buf();
    c.bench_function("session_search_listing_1k_outlier", |b| {
        b.iter(|| black_box(SessionManager::list_sessions(&large_dir, None)))
    });
    assert_eq!(
        session_listing_corpus_bytes(),
        0,
        "timed listings must never materialize message corpora"
    );
}

fn bench_payload(round: u64) -> String {
    const PAYLOAD_BYTES: usize = 1024 * 1024;
    let prefix = format!("round-{round:020}");
    let mut payload = String::with_capacity(PAYLOAD_BYTES);
    payload.push_str(&prefix);
    payload.push_str(&"x".repeat(PAYLOAD_BYTES - prefix.len()));
    payload
}

/// Document commit with no observer (T2-1, R1-L3-014).
///
/// `Harness::doc` loads the cached document, applies the edit and commits it.
/// No `document_state` attachment, no watch and no commit subscriber exists,
/// so the line must skip `assemble::adopt` entirely: the clone counter has to
/// stay at zero while each timed commit still writes a 1 MiB payload through
/// the real `JsonlStorage`. A `current_thread` runtime keeps the thread-local
/// counter and the commit on the bench thread.
///
/// Revision control: the document is the built-in `pi.usage` definition,
/// whose `checkpoint_when` returns true. Every changed draft therefore commits
/// a complete Base, which `MemoryStorage` stores as a single revision and
/// `doc-*.jsonl` rewrites in place. With a custom kind the harness's
/// `BuiltinBehaviours` would return no predicate and each unique payload would
/// append a Delta forever (~4.5 GB over a 5 s run). Timed setup generates a
/// distinct 1 MiB payload per iteration (a round counter in the first bytes),
/// so `plan_document()` sees a real value change and produces an operation
/// batch instead of the no-op `content: None` path.
///
/// Correctness gates: a separate untimed observer control commits the same
/// edit shape and requires `document_publication_clones() > 0`; the timed
/// no-observer run then requires it to be zero, and the usage document's
/// sidecar bytes must change between the pre-bench snapshot and the end of the
/// timed loop (at least one real document write).
fn durable_document_commit_no_observer(c: &mut Criterion) {
    use pir::durable::harness::{usage_address, USAGE_DEFINITION};
    use pir::durable::session::assemble::{
        document_publication_clones, reset_document_publication_clones,
    };
    use pir::durable::DocumentPoint;

    let fixture = BenchFixture::new("durable-document-no-observer");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("durable document bench runtime");
    let storage: Arc<dyn Storage> = Arc::new(
        runtime
            .block_on(JsonlStorage::open(fixture.dir()))
            .expect("open durable document fixture"),
    );
    let harness = runtime
        .block_on(Harness::open(
            Arc::clone(&storage),
            HarnessOptions::default(),
            DurableContext::background(),
        ))
        .expect("open durable document harness");
    let conversation = runtime
        .block_on(harness.root(DurableContext::background()))
        .expect("create root conversation");
    let resolved = usage_address(conversation.id());
    let definition = (*USAGE_DEFINITION).clone();
    let context = DurableContext::background();

    let snapshot_path = {
        let record = runtime
            .block_on(storage.find_document(resolved.address.clone(), DocumentPoint::Current))
            .expect("find usage document")
            .expect("root conversation must stage a pi.usage document");
        fixture.dir().join(format!("doc-{}.jsonl", record.id))
    };
    assert!(
        snapshot_path.is_file(),
        "usage document sidecar must exist before the timed run: {}",
        snapshot_path.display()
    );

    runtime
        .block_on(harness.doc(
            resolved.clone(),
            definition.clone(),
            context.clone(),
            move |draft| {
                draft.insert(
                    "payload".to_owned(),
                    serde_json::Value::String(bench_payload(0)),
                );
            },
        ))
        .expect("create bench document without observer");

    // Observer control: the same edit shape must be publishable. This proves
    // the payload change is a real plan (not the no-op path) and that the
    // clone counter is sensitive to an attached observer.
    let attached = runtime
        .block_on(harness.document_state(&resolved, &definition, &context))
        .expect("attach document state control")
        .expect("usage document must be live");
    reset_document_publication_clones();
    runtime
        .block_on(harness.doc(
            resolved.clone(),
            definition.clone(),
            context.clone(),
            move |draft| {
                draft.insert(
                    "payload".to_owned(),
                    serde_json::Value::String(bench_payload(1)),
                );
            },
        ))
        .expect("observer control commit");
    let observer_clones = document_publication_clones();
    assert!(
        observer_clones > 0,
        "observer control must clone the published document revision; the bench \
         would otherwise be measuring a no-op"
    );
    runtime
        .block_on(harness.detach(&attached))
        .expect("detach document state control");
    eprintln!(
        "[bench] durable_document_commit_no_observer observer control: \
         publication_clones={observer_clones}"
    );
    reset_document_publication_clones();

    let sidecar_before =
        std::fs::read(&snapshot_path).expect("read document sidecar before timed run");
    let mut round = 1u64;
    c.bench_function("durable_document_commit_no_observer", |b| {
        b.iter_batched(
            || {
                round += 1;
                (
                    bench_payload(round),
                    definition.clone(),
                    resolved.clone(),
                    context.clone(),
                )
            },
            |(payload, definition, resolved, context)| {
                runtime
                    .block_on(harness.doc(resolved, definition, context, move |draft| {
                        draft.insert("payload".to_owned(), serde_json::Value::String(payload));
                    }))
                    .expect("commit document without observer");
            },
            BatchSize::PerIteration,
        )
    });

    // Criterion's list mode registers the benchmark without running any
    // iteration (`round` stays at its initial value); only enforce the write
    // gate when the timed loop actually ran.
    if round > 1 {
        let sidecar_after =
            std::fs::read(&snapshot_path).expect("read document sidecar after timed run");
        assert_ne!(
            sidecar_before, sidecar_after,
            "timed iterations must write the document sidecar; identical bytes mean \
             every edit was the no-op `content: None` path (rounds={round})"
        );
    }
    assert_eq!(
        document_publication_clones(),
        0,
        "a no-observer document commit must not clone a full value or operation batch"
    );

    runtime
        .block_on(harness.close())
        .expect("close durable document harness");
}

/// Wrapper so the live `ToolExecutionComponent` (which owns interior mutable
/// state and an `update_result` API) can sit in `TuiCore`'s boxed component
/// list while the bench keeps an `Arc` handle to dirty it between frames.
struct SharedToolComponent(Arc<ToolExecutionComponent>);

impl Component for SharedToolComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.0.render(width)
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.0.render_into(width, out)
    }
}

/// TUI dirty frame with a 512-entry transcript (R1-L4-008, plus the R1-L4-013
/// frame-cadence path): 511 settled `AssistantMessageComponent`s (20 rows and
/// one 2 KiB body line each) plus one live tool-result block that is dirtied
/// every frame. `TuiCore::render` still assembles and diffs the whole
/// transcript; the cached assistant rows hit their segment caches, so this
/// measures the per-dirty-frame band rebuild + `Rows::push` scan rather than
/// the steady-state cache-hit number `tui_steady_frame_assembly` reports.
/// Setup: push all components, prime the caches with two warm frames. Timed:
/// one `update_result` in `iter_batched_ref` setup (changing the tool output,
/// untimed) followed by `TuiCore::render` alone.
fn tui_dirty_frame(c: &mut Criterion) {
    let terminal = Terminal::with_sink_factory(
        || Box::new(std::io::sink()) as Box<dyn TerminalSink>,
        WriteLog::disabled(),
    );
    let mut core = TuiCore::new(terminal);
    core.terminal().set_size(120, 40);
    for index in 0..511 {
        let mut text = String::new();
        for row in 0..19 {
            text.push_str(&format!("assistant row {row:02} {}\n", "x".repeat(72)));
        }
        text.push_str(&format!("tool result {index:03} {}", "y".repeat(2000)));
        core.push_content(Box::new(AssistantMessageComponent::new_from(
            Arc::new(assistant(&text)),
            false,
            0,
        )));
    }
    let dirty = Arc::new(ToolExecutionComponent::new(
        "bench",
        Arc::new(json!({ "command": "bench" })),
        Arc::new(AtomicBool::new(false)),
    ));
    let dirty_output = |frame: usize| format!("dirty tool result {frame:08} {}", "z".repeat(1900));
    dirty.update_result(Arc::from(dirty_output(0)), false, Some(0));
    core.push_content(Box::new(SharedToolComponent(Arc::clone(&dirty))));
    // Warm the segment caches and row pools so the measured frames are the
    // steady dirty path, not first-render markdown parsing.
    core.render();
    core.render();
    let mut frame = 0usize;
    c.bench_function("tui_dirty_frame_512", |b| {
        b.iter_batched_ref(
            || {
                frame += 1;
                dirty.update_result(Arc::from(dirty_output(frame)), false, Some(frame as u64));
            },
            |_| {
                core.render();
                black_box(core.last_frame_rows_work())
            },
            BatchSize::PerIteration,
        )
    });
    core.stop();
}

criterion_group!(
    benches,
    protocol_cbor,
    frame_decode,
    scheduler,
    sse_event_stream,
    harness_fixed_costs,
    tool_registry_build,
    tool_dispatch,
    tool_read_file,
    tool_edit_file,
    tool_bash_spawn,
    tool_bash_call,
    extension_tool_bridge,
    tui_frame_assembly,
    server_fanout_encode,
    sse_google_parse,
    durable_session_replay,
    durable_session_append,
    session_open_long_line,
    durable_context_read,
    tui_dirty_frame
);

// The large-fixture group below holds B3/B8/T2-1 plus the 10 MB B3 control,
// the B8 cold-keystroke path and the B8 real-directory listing pair. A second
// group keeps their run cost bounded without changing the 23 existing benches'
// sampling parameters: 10 samples, 1 s warm-up, 5 s measurement. A CLI
// `--sample-size` / `--measurement-time` still overrides these through
// `configure_from_args`.
criterion_group!(
    name = large_fixture_benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5));
    targets =
        jsonl_recovery_open,
        jsonl_recovery_open_10mb,
        print_paged_prompt_context_100mb,
        session_search_keystroke,
        session_search_listing,
        durable_document_commit_no_observer
);

criterion_main!(benches, large_fixture_benches);
