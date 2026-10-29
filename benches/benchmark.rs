use std::process::Stdio;
use std::sync::Arc;

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
use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use pir::protocol::{
    decode_value, encode_cbor, encode_frame, encode_server_envelope, CborOptions, FrameDecoder,
    FrameOptions, ServerEnvelope, ServiceUpdateFrame,
};
use pir::tui::core::{Component, Rows, Terminal, TerminalSink, TuiCore};
use pir::tui::stdin_buffer::WriteLog;
use serde_json::json;

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
    durable_session_replay
);
criterion_main!(benches);
