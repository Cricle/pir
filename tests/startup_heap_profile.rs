//! Heap profiling harness for the debug startup / agent-session path.
//!
//! This test is ignored by default because it installs the `dhat` global
//! allocator for the whole test binary and writes a DHAT JSON profile. Run it
//! explicitly:
//!
//! ```text
//! cargo test --test startup_heap_profile --locked -- --ignored --nocapture
//! ```
//!
//! The profile path is printed as a `[dhat] profile:` line; open the JSON in
//! DHAT's viewer (or `dhat-viewer`) to inspect allocation sites. The test is
//! hermetic: the model is a minimal in-process [`Provider`], the agent uses
//! [`SessionMode::None`], and the working/agent directories are a per-test
//! temp directory, so no network, API keys, or real `~/.pi` state are touched.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Model, ModelCost,
    ModelCostRates, StopReason, StreamOptions, TerminalReason, TextContent, ThinkingLevel, Usage,
};
use pir::sdk::{AgentBuilder, SessionMode};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

// ---------------------------------------------------------------------------
// Minimal scripted in-process provider (same pattern as tests/sdk_extended.rs)
// ---------------------------------------------------------------------------

struct ScriptedProvider {
    calls: AtomicUsize,
}

fn mock_model() -> Model {
    Model {
        id: "scripted-model".into(),
        name: "Scripted model".into(),
        api: "scripted-api".into(),
        provider: "scripted".into(),
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
        context_window: 8_192,
        max_tokens: 512,
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

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "scripted".into(),
        provider: "scripted".into(),
        model: "scripted-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        usage: Usage::default(),
        ..Default::default()
    }
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "scripted"
    }

    fn name(&self) -> &str {
        "Scripted provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![mock_model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        // `with_registry` streams unchecked, so no key source is required.
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let message = assistant(
            vec![ContentBlock::Text(TextContent {
                text: "profile ok".to_owned(),
                text_signature: None,
            })],
            StopReason::Stop,
        );
        let stream = AssistantMessageEventStream::new();
        if let Some(ContentBlock::Text(text)) = message.content.first() {
            stream.push(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: text.text.clone(),
                partial: Arc::new(message.clone()),
            });
        }
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

// ---------------------------------------------------------------------------
// Fixtures / reporting
// ---------------------------------------------------------------------------

/// Per-test temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-startup-heap-{label}-{}-{}",
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

/// Print the six DHAT heap counters for one snapshot.
fn print_stats(label: &str, stats: &dhat::HeapStats) {
    println!(
        "[dhat] {label}: curr_bytes={} curr_blocks={} total_bytes={} total_blocks={} \
         max_bytes={} max_blocks={}",
        stats.curr_bytes,
        stats.curr_blocks,
        stats.total_bytes,
        stats.total_blocks,
        stats.max_bytes,
        stats.max_blocks
    );
}

fn stats_delta(before: &dhat::HeapStats, after: &dhat::HeapStats) -> (i128, i128) {
    (
        after.curr_bytes as i128 - before.curr_bytes as i128,
        after.curr_blocks as i128 - before.curr_blocks as i128,
    )
}

/// `PIR_HEAP_SCENARIO=minimal` (default) keeps the original hermetic build:
/// no core tools, no plugins. `builtin` adds the shape the interactive host
/// uses — the seven core tools plus every default-enabled Rust builtin — so
/// the plugin host, its tool definitions, and the system prompt's tool list
/// are in the measured set. `session` is `builtin` plus a persisted session
/// fixture (`PIR_SESSION_MB`, default 2) opened tail-first through
/// [`SessionMode::Existing`] — the interactive host's startup shape — and
/// exercises the attach-replay / load-more / widen-per-prompt reads so the
/// "2 MB session projection/render/request" delta is attributable by
/// allocation site. `js` is `builtin` plus one minimal JS extension (the
/// same shape `tui_session_probe.py --with-js-extension` measures at
/// ~+25 MB tree RSS) so the QuickJS runtime / prelude / transpile / handler
/// tables are attributable by allocation site.
fn scenario() -> String {
    std::env::var("PIR_HEAP_SCENARIO").unwrap_or_else(|_| "minimal".to_owned())
}

/// The minimal JS extension the `js` scenario loads through
/// [`AgentBuilder::extension_paths`] — byte-identical in shape to the probe
/// fixture (`scripts/perf/tui_session_probe.py --with-js-extension`): one
/// tool, one command, one `session_start` hook.
const JS_PROBE_EXTENSION: &str = "export default function (pi) {
  pi.registerTool({
    name: \"js_probe\",
    description: \"Probe tool\",
    parameters: { type: \"object\", properties: {} },
    async execute() { return { content: [{ type: \"text\", text: \"ok\" }] }; },
  });
  pi.registerCommand(\"jsprobe\", { handler: async () => {} });
  pi.on(\"session_start\", async () => {});
}
";

/// `PIR_HEAP_STAGE=build` stops after the agent is built and writes a profile
/// that only contains the construction allocations; the default `prompt`
/// continues through one prompt. `session` scenarios additionally run the
/// attach-replay (`transcript`) and load-more reads before the prompt, each
/// bracketed by its own heap snapshot.
fn stage() -> String {
    std::env::var("PIR_HEAP_STAGE").unwrap_or_else(|_| "prompt".to_owned())
}

/// Session fixture size for the `session` scenario (`PIR_SESSION_MB`, default
/// 2 — the size RESULTS.md attributes ~14.9 MB of TUI startup delta to).
fn session_mb() -> u64 {
    std::env::var("PIR_SESSION_MB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2)
}

/// Write a valid v3 JSONL session of roughly `size_mb` MB, byte-shape-identical
/// to `scripts/perf/big_session_probe.py:make_session` (user/assistant text
/// pairs, model/thinking bookkeeping first, growing usage records) so the dhat
/// attribution matches the ConPTY probe's fixture. Returns the bytes written.
fn write_session_fixture(path: &Path, size_mb: u64, cwd: &str) -> u64 {
    use std::io::Write as _;
    let target = size_mb * 1024 * 1024;
    let stamp = "2026-01-01T00:00:00.000Z";
    let payload = "lorem ipsum dolor sit amet consectetur adipiscing elit "
        .repeat(20)
        .trim()
        .to_owned();
    let mut written = 0u64;
    let mut file = std::fs::File::create(path).expect("create session fixture");
    let line = |entry: serde_json::Value, file: &mut std::fs::File| -> u64 {
        let mut json = serde_json::to_string(&entry).expect("serialize fixture entry");
        json.push('\n');
        file.write_all(json.as_bytes())
            .expect("write fixture entry");
        json.len() as u64
    };
    written += line(
        serde_json::json!({"type": "session", "version": 3, "id": "fixture-session",
                           "timestamp": stamp, "cwd": cwd}),
        &mut file,
    );
    written += line(
        serde_json::json!({"type": "model_change", "id": "m0", "parentId": serde_json::Value::Null,
                           "timestamp": stamp, "provider": "mock", "modelId": "mock-model"}),
        &mut file,
    );
    written += line(
        serde_json::json!({"type": "thinking_level_change", "id": "t0", "parentId": "m0",
                           "timestamp": stamp, "thinkingLevel": "off"}),
        &mut file,
    );
    let mut index = 0u64;
    let mut parent = "t0".to_owned();
    while written < target {
        index += 1;
        let user_id = format!("u{index:06}");
        let asst_id = format!("a{index:06}");
        let usage = serde_json::json!({
            "input": 1_500_000 + index * 250, "output": 120, "cacheRead": 0,
            "cacheWrite": 0, "totalTokens": 1_500_000 + index * 250 + 120,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}
        });
        written += line(
            serde_json::json!({"type": "message", "id": user_id, "parentId": parent,
                               "timestamp": stamp,
                               "message": {"role": "user", "timestamp": 0,
                                           "content": [{"type": "text",
                                                        "text": format!("question {index}: {payload}")}]}}),
            &mut file,
        );
        written += line(
            serde_json::json!({"type": "message", "id": asst_id, "parentId": user_id,
                               "timestamp": stamp,
                               "message": {"role": "assistant", "timestamp": 0,
                                           "api": "openai-completions", "provider": "mock",
                                           "model": "mock-model", "stopReason": "stop",
                                           "usage": usage,
                                           "content": [{"type": "text",
                                                        "text": format!("answer {index}: {payload}")}]}}),
            &mut file,
        );
        parent = asst_id;
    }
    file.sync_all().expect("flush session fixture");
    written
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

#[test]
#[ignore = "heap profiling harness; run explicitly"]
fn startup_heap_profile() {
    let profile_path = std::env::temp_dir().join(format!(
        "pir-startup-heap-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let profiler = dhat::Profiler::builder().file_name(&profile_path).build();
    println!("[dhat] profile: {}", profile_path.display());
    println!(
        "[dhat] build: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("[dhat] scenario: {} stage: {}", scenario(), stage());

    let cwd = TempDir::new("profile");
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(ScriptedProvider {
        calls: AtomicUsize::new(0),
    }));
    let registry = Arc::new(registry);

    // Real SDK path: same builder shape as the hermetic tests in
    // tests/sdk_extended.rs, but without session storage. `.agent_dir(cwd)`
    // keeps settings/plugin discovery inside the temp directory instead of
    // reading the developer's real `~/.pi`.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    // Baseline: whatever the profiler, test harness, and tokio runtime have
    // live before any SDK work starts.
    let before_build = dhat::HeapStats::get();
    print_stats("before agent build", &before_build);

    let builtin_scenario = scenario() == "builtin" || scenario() == "session" || scenario() == "js";
    let builtins: Vec<&'static str> = if builtin_scenario {
        pir::plugins::BUILTIN_EXTENSION_FACTORIES
            .iter()
            .filter(|def| def.default_enabled)
            .map(|def| def.name)
            .collect()
    } else {
        Vec::new()
    };
    // The `js` scenario loads the probe extension through the same explicit
    // path the CLI's `-e` uses, so the measured set is exactly
    // `builtin` + (QuickJS runtime + prelude + transpile + this plugin).
    let js_extension: Vec<PathBuf> = if scenario() == "js" {
        let path = cwd.path().join("js-probe.js");
        std::fs::write(&path, JS_PROBE_EXTENSION).expect("write js probe extension");
        vec![path]
    } else {
        Vec::new()
    };
    // The `session` scenario persists the fixture through the SAME tail-first
    // open the interactive host uses (`SessionMode::Existing` →
    // `open_session_tail_first` → `open_paged`), so the build delta carries
    // the paged-open cost and the later stages carry the attach replay,
    // load-more, and widen-per-prompt reads.
    let session_fixture = TempDir::new("session-fixture");
    let session_path = session_fixture.path().join("tui-big.jsonl");
    if scenario() == "session" {
        let bytes = write_session_fixture(
            &session_path,
            session_mb(),
            cwd.path().to_str().unwrap_or("."),
        );
        println!(
            "[dhat] session fixture: {} ({bytes} bytes)",
            session_path.display()
        );
    }
    let agent = runtime.block_on(async {
        let mut builder = AgentBuilder::new()
            .with_registry(Arc::clone(&registry), mock_model())
            .cwd(cwd.path())
            .agent_dir(cwd.path())
            .no_compaction()
            .thinking_level(ThinkingLevel::Off)
            .core_tools(builtin_scenario)
            .builtins(builtins)
            .extension_paths(js_extension);
        builder = if scenario() == "session" {
            builder.session(SessionMode::Existing(session_path.clone()))
        } else {
            builder.session(SessionMode::None)
        };
        builder.build().await.expect("build agent")
    });

    let after_build = dhat::HeapStats::get();
    print_stats("after agent build", &after_build);
    let (live_bytes, live_blocks) = stats_delta(&before_build, &after_build);
    println!("[dhat] agent build delta: curr_bytes={live_bytes:+} curr_blocks={live_blocks:+}");

    if stage() == "build" {
        // Flush while the built agent is still live: the profile's end-of-run
        // snapshot is then exactly the construction cost.
        drop(profiler);
        let profile_len = std::fs::metadata(&profile_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        println!(
            "[dhat] profile written (build stage): {} ({profile_len} bytes)",
            profile_path.display()
        );
        assert!(
            profile_path.exists(),
            "dhat profile was not written to {}",
            profile_path.display()
        );
        return;
    }

    // Attach-replay shape (the TUI's `replay_transcript_entries`): one
    // projection materialization of the resident window. Measured separately
    // so the projection clone's cost is attributable.
    if scenario() == "session" {
        runtime.block_on(async {
            let before_replay = dhat::HeapStats::get();
            let replayed = agent.transcript().await;
            let after_replay = dhat::HeapStats::get();
            print_stats("after attach replay (transcript)", &after_replay);
            let (live_bytes, live_blocks) = stats_delta(&before_replay, &after_replay);
            println!(
                "[dhat] attach replay delta: curr_bytes={live_bytes:+} curr_blocks={live_blocks:+} messages={}",
                replayed.len()
            );
            drop(replayed);

            // Load-more shape (the TUI's scroll-up `LoadOlder`): bounded older
            // pages with the session layer's default caps, until the file start.
            let before_pages = dhat::HeapStats::get();
            let mut cursor = agent.transcript_cursor().await;
            let mut pages = 0usize;
            let mut page_messages = 0usize;
            while let Some(at) = cursor {
                let Some((messages, next)) = agent
                    .older_transcript_messages(
                        at,
                        pir::session_manager::DEFAULT_TAIL_PAGE_BYTES,
                        pir::session_manager::DEFAULT_TAIL_PAGE_ENTRIES,
                    )
                    .await
                    .expect("read older page")
                else {
                    break;
                };
                page_messages += messages.len();
                pages += 1;
                cursor = if next < at { Some(next) } else { None };
            }
            let after_pages = dhat::HeapStats::get();
            print_stats("after load-more pages", &after_pages);
            let (live_bytes, live_blocks) = stats_delta(&before_pages, &after_pages);
            println!(
                "[dhat] load-more delta: curr_bytes={live_bytes:+} curr_blocks={live_blocks:+} \
                 pages={pages} page_messages={page_messages} cursor_left={}",
                cursor.is_some()
            );
        });
    }

    let outcome = runtime
        .block_on(agent.prompt("profile this startup path"))
        .expect("one prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("profile ok"));

    let after_prompt = dhat::HeapStats::get();
    print_stats("after one prompt", &after_prompt);
    let (live_bytes, live_blocks) = stats_delta(&after_build, &after_prompt);
    println!("[dhat] prompt delta: curr_bytes={live_bytes:+} curr_blocks={live_blocks:+}");

    // Dropping the profiler flushes the DHAT JSON profile to `profile_path`.
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
