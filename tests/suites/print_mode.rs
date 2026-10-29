//! Print-mode host units (task G3): the threshold-compaction config
//! resolution (settings `compaction` + per-model override), the `@file`
//! image/text partition (`initialImages`), the extension-command dispatch
//! gating that backs the print host's commandContextActions surface, the
//! plugin-host routes the print run installs (`print_transport_hooks`, the
//! session-context model push, and the `message_end_repair` loop seam), the
//! threshold-compaction lifecycle the JSON stream carries
//! ([`print_compaction_hook`]: `compaction_start` / `compaction_end`), and
//! `background: true` in the one-shot host (the background section: real
//! detach/delivery through [`PrintBackground`], the live follow-up injection,
//! the text/json surfaces, and the bounded exit settle).
//!
//! Fixtures are generated into per-test temp directories (the repository
//! hygiene gate forbids tracked `.ts` files and tests never touch the real
//! `~/.pi` directories).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::agent::agent_loop::{
    run_with_options, LoopDeps, LoopOptions, MessageEndRepairContext, MessageEndRepairFn,
    PersistMessageFn, ProviderRequest, StreamFn,
};
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::agent_session::SessionEvent;
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, TextContent, ThinkingLevel, ToolCall, Usage, UserMessage,
    UserRole,
};
use pir::coding_agent::json::to_session_event_json;
use pir::coding_agent::print::{
    background_abort_diagnostic, background_abort_event, partition_file_args,
    print_apply_max_turns, print_apply_retry, print_compaction_config, print_compaction_hook,
    print_loop_deps, print_plugin_host, print_retry_policy, print_tool_definitions,
    print_transport_hooks, text_with_background_reports, threshold_model_for,
    try_extension_command, PrintBackground, PrintSessionEventSink, SharedSession,
};
use pir::compaction::{CompactionConfig, PathEntryMeta, ProjectionEntryMeta};
use pir::extensions::{ExtensionConfig, ExtensionHandle, SessionContextSnapshot};
use pir::plugins::builtin::instantiate_builtin;
use pir::plugins::{AgentPlugin, PluginHost};
use pir::session_manager::{
    AppendFailure, CustomMessageEntry, FileEntry, SessionManager, StoredMessage,
};

// ---------------------------------------------------------------------------
// Fixtures and fakes
// ---------------------------------------------------------------------------

/// Per-test temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-print-mode-{label}-{}-{}",
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_model() -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: "openai-completions".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
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
        context_window: 128_000,
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

fn settings_from_json(value: serde_json::Value) -> pir::config::settings::Settings {
    serde_json::from_value(value).expect("parse settings fixture")
}

async fn load_runtime_with_command(dir: &TempDir, command_source: &str) -> ExtensionHandle {
    let source = format!("export default function (pi) {{\n{command_source}\n}}\n");
    let path = dir.0.join("ext.ts");
    std::fs::write(&path, source).expect("write fixture");
    let runtime = ExtensionHandle::start(ExtensionConfig {
        cwd: dir.0.clone(),
        mode: "print".to_string(),
        has_ui: false,
        model: Some("fake-model".to_string()),
        session_id: Some("print-mode-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("extension runtime");
    let result = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load extensions");
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
    runtime
}

// ---------------------------------------------------------------------------
// Compaction config resolution
// ---------------------------------------------------------------------------

#[test]
fn compaction_config_defaults_when_settings_empty() {
    let settings = settings_from_json(serde_json::json!({}));
    let config =
        print_compaction_config(&settings, &test_model()).expect("compaction enabled by default");
    // Upstream SETTINGS_DEFAULTS.compaction.
    assert!(config.enabled);
    assert_eq!(config.reserve_tokens, 16_384);
    assert_eq!(config.keep_recent_tokens, 20_000);
}

#[test]
fn compaction_config_disabled_returns_none() {
    let settings = settings_from_json(serde_json::json!({ "compaction": { "enabled": false } }));
    assert!(print_compaction_config(&settings, &test_model()).is_none());
}

#[test]
fn compaction_config_applies_model_override() {
    let settings = settings_from_json(serde_json::json!({
        "compaction": {
            "enabled": true,
            "reserveTokens": 8_192,
            "keepRecentTokens": 10_000,
            "modelOverrides": {
                "test/test-model": { "reserveTokens": 1_024 }
            }
        }
    }));
    let config = print_compaction_config(&settings, &test_model()).expect("compaction enabled");
    // Per-model override wins for reserveTokens; the ordinary setting holds
    // for keepRecentTokens (SettingsManager `compactionToken` semantics).
    assert_eq!(config.reserve_tokens, 1_024);
    assert_eq!(config.keep_recent_tokens, 10_000);
}

#[test]
fn compaction_config_other_model_uses_ordinary_settings() {
    let settings = settings_from_json(serde_json::json!({
        "compaction": {
            "enabled": true,
            "reserveTokens": 8_192,
            "modelOverrides": {
                "other/other-model": { "reserveTokens": 1_024 }
            }
        }
    }));
    let config = print_compaction_config(&settings, &test_model()).expect("compaction enabled");
    assert_eq!(config.reserve_tokens, 8_192);
}

// ---------------------------------------------------------------------------
// Threshold model (virtual routing fallback)
// ---------------------------------------------------------------------------

#[test]
fn threshold_model_for_physical_model_is_identity() {
    let registry = pir::ai::provider::ProviderRegistry::default();
    let model = test_model();
    let threshold = threshold_model_for(&registry, &model);
    assert_eq!(threshold.provider, model.provider);
    assert_eq!(threshold.id, model.id);
    assert_eq!(threshold.context_window, model.context_window);
}

// ---------------------------------------------------------------------------
// `@file` partitioning (initialImages)
// ---------------------------------------------------------------------------

#[test]
fn partition_file_args_splits_images_from_text() {
    let dir = TempDir::new("partition");
    let cwd: &Path = &dir.0;
    // A minimal PNG head (8-byte signature + IHDR length/type) and a text file.
    let png = dir.0.join("shot.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDRfake-body").expect("write png");
    let text = dir.0.join("notes.txt");
    std::fs::write(&text, "hello").expect("write text");

    let (file_text, images) =
        partition_file_args(&["shot.png".to_string(), "notes.txt".to_string()], cwd)
            .expect("partition");
    // The image positional is consumed as an attachment AND still contributes
    // its upstream marker line; the text positional renders through
    // process_file_arguments at its original position.
    assert_eq!(
        file_text,
        format!(
            "<file name=\"{}\"></file>\n<file name=\"{}\">\nhello\n</file>\n",
            png.display(),
            text.display()
        )
    );
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].mime_type, "image/png");
    let expected = base64_of(&std::fs::read(&png).expect("read png"));
    assert_eq!(images[0].data, expected);
}

#[test]
fn partition_file_args_missing_file_is_error() {
    let dir = TempDir::new("missing");
    let error = partition_file_args(&["nope.png".to_string()], &dir.0)
        .expect_err("missing file is a hard error");
    assert!(error.to_string().contains("File not found"), "{error}");
}

#[test]
fn partition_file_args_empty_is_all_text() {
    let (file_text, images) = partition_file_args(&[], Path::new(".")).expect("empty partition");
    assert!(file_text.is_empty());
    assert!(images.is_empty());
}

/// Unreadable garbage that matches no image signature stays on the text path.
#[test]
fn partition_file_args_binary_non_image_goes_to_text() {
    let dir = TempDir::new("binary");
    let bin = dir.0.join("blob.bin");
    std::fs::write(&bin, [0x00, 0x01, 0x02, 0x03]).expect("write blob");
    let (file_text, images) =
        partition_file_args(&["blob.bin".to_string()], &dir.0).expect("partition");
    assert!(
        file_text.contains(&format!("<file name=\"{}\">", bin.display())),
        "{file_text}"
    );
    assert!(images.is_empty());
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ---------------------------------------------------------------------------
// Extension command dispatch (commandContextActions surface)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extension_command_dispatch_consumes_registered_command() {
    let dir = TempDir::new("command");
    let runtime = load_runtime_with_command(
        &dir,
        r#"pi.registerCommand("greet", { handler: async () => {} });"#,
    )
    .await;
    // A registered command consumes the turn ("handled", upstream prompt()
    // semantics) and the dispatch resolves ok. The session storage is empty
    // (no context actions are requested), which the dispatch tolerates.
    let session: pir::coding_agent::print::SharedSession =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let session_dir = dir.0.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    assert!(try_extension_command(&runtime, "/greet world", &session, &session_dir, None).await);
    // "/" alone and unknown commands fall through to the model.
    assert!(!try_extension_command(&runtime, "/", &session, &session_dir, None).await);
    assert!(
        !try_extension_command(&runtime, "/unknown-command", &session, &session_dir, None).await
    );
    assert!(!try_extension_command(&runtime, "plain prompt", &session, &session_dir, None).await);
}

#[tokio::test]
async fn extension_command_handler_error_is_reported_not_swallowed() {
    let dir = TempDir::new("command-error");
    let runtime = load_runtime_with_command(
        &dir,
        r#"pi.registerCommand("boom", { handler: async () => { throw new Error("kaboom"); } });"#,
    )
    .await;
    // The handler throws; the turn is still consumed (upstream marks the
    // command handled and emits the error) — the dispatch surfaces the error
    // in the outcome, and try_extension_command keeps returning true.
    let session: pir::coding_agent::print::SharedSession =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let session_dir = dir.0.join("sessions");
    assert!(try_extension_command(&runtime, "/boom", &session, &session_dir, None).await);
}

#[tokio::test]
async fn extension_command_without_extensions_is_pass_through() {
    // No extension file: every prompt falls through to the model.
    let dir = TempDir::new("no-ext");
    let runtime = ExtensionHandle::start(ExtensionConfig {
        cwd: dir.0.clone(),
        mode: "print".to_string(),
        ..ExtensionConfig::default()
    })
    .expect("extension runtime");
    runtime.load(&[]).await.expect("load nothing");
    let session: pir::coding_agent::print::SharedSession =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let session_dir = dir.0.join("sessions");
    assert!(!try_extension_command(&runtime, "/reload", &session, &session_dir, None).await);
    assert!(!try_extension_command(&runtime, "/anything", &session, &session_dir, None).await);
}

// ---------------------------------------------------------------------------
// Plugin-host routes: `before_provider_request` + the session-context push
// ---------------------------------------------------------------------------
//
// Print drives `agent_loop` itself, so its per-request hooks are built by
// `print_transport_hooks` from the SAME unified plugin host the session's
// streamFn seam uses (`PluginHost::emit`): the Rust builtins (`tool-repair`'s
// Phase 0 anchor sanitizer) and the JS adapter over the runtime handle the
// host owns dispatch in ONE ordered walk. These tests pin the sanitization
// through that route, the exactly-once JS dispatch (a hand-rolled
// `runtime.emit` on top would double-count), and the context push the native
// gate reads.

/// A `tool-repair` builtin instance (the construction the tool-repair suite
/// uses).
fn tool_repair_builtin() -> Arc<dyn AgentPlugin> {
    instantiate_builtin("tool-repair")
        .expect("builtin:tool-repair must be registered (src/plugins/builtin.rs:114)")
}

/// The OpenAI-family tool slot pir's `convert_tool` emits, carrying one
/// anchored string schema (`^foo$` — an `anchorBleedModels` repair target).
fn anchored_body() -> serde_json::Value {
    serde_json::json!({
        "model": "gated-model",
        "tools": [{
            "type": "function",
            "function": {
                "name": "read",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string", "pattern": "^foo$"}},
                    "required": ["path"],
                },
            },
        }],
    })
}

/// The `pattern` value of [`anchored_body`]'s one tool schema.
fn tool_pattern(body: &serde_json::Value) -> &serde_json::Value {
    &body["tools"][0]["function"]["parameters"]["properties"]["path"]["pattern"]
}

/// The model snapshot the print run pushes (`print_model_snapshot`'s shape:
/// the serialized `Model` object).
fn model_snapshot(id: &str) -> SessionContextSnapshot {
    SessionContextSnapshot {
        model: serde_json::json!({ "id": id, "provider": "test" }),
        project_trusted: true,
        ..SessionContextSnapshot::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn print_transport_hooks_route_the_native_before_provider_request_face() {
    // A host with the tool-repair builtin and NO JS runtime (the
    // plugin-only shape): the builtin's declared interest is what installs
    // the print request hook.
    let host = Arc::new(PluginHost::build(None, vec![tool_repair_builtin()]));
    let hooks = print_transport_hooks(Some(&host), None);
    assert!(
        hooks.before_provider_request.is_some(),
        "the native interest must install the print request hook"
    );
    assert!(
        hooks.after_provider_response.is_none(),
        "tool-repair registers no response observer"
    );

    // No pushed model: the gate is closed and the body round-trips untouched.
    let body = anchored_body();
    let returned = hooks.before_provider_request.as_ref().unwrap()(body.clone()).await;
    assert_eq!(returned, body, "no pushed model: no repair");

    // Gated model (the `anchorBleedModels` set): the sanitized body is the
    // merged result the transport substitutes.
    host.update_session_context(model_snapshot("glm-4.6"));
    let returned = hooks.before_provider_request.as_ref().unwrap()(body.clone()).await;
    assert_eq!(tool_pattern(&returned), &serde_json::json!("foo"));
    // Everything but the pattern survives.
    assert_eq!(
        returned["tools"][0]["function"]["name"],
        serde_json::json!("read")
    );

    // A non-gated model turns the face back off (byte-identical round-trip).
    host.update_session_context(model_snapshot("gpt-4.1"));
    let returned = hooks.before_provider_request.as_ref().unwrap()(body.clone()).await;
    assert_eq!(returned, body);

    // No host at all: the JS-only fallback seam stays unset without a
    // runtime/handlers (the pre-existing behavior).
    let fallback = print_transport_hooks(None, None);
    assert!(fallback.before_provider_request.is_none());
    assert!(fallback.after_provider_response.is_none());
}

/// A JS extension whose `before_provider_request` handler counts its
/// dispatches and returns a replacement; the response observer records the
/// status.
const JS_HOOK_EXTENSION: &str = r#"
  globalThis.__bprCount = 0;
  globalThis.__respStatus = null;
  pi.on("before_provider_request", (event) => {
    globalThis.__bprCount += 1;
    return { ...event.payload, jsSeen: true };
  });
  pi.on("after_provider_response", (event) => {
    globalThis.__respStatus = event.status;
  });
"#;

#[tokio::test(flavor = "current_thread")]
async fn print_transport_hooks_merge_native_and_js_exactly_once() {
    let dir = TempDir::new("host-route");
    let runtime = load_runtime_with_command(&dir, JS_HOOK_EXTENSION).await;
    // The production print assembly: `order_extension_paths` inserts every
    // default `builtin:<name>` ahead of the default file paths, so the
    // tool-repair builtin and the JS adapter share ONE ordered dispatch list.
    runtime.set_path_plan(vec![
        PathBuf::from("builtin:tool-repair"),
        dir.0.join("ext.ts"),
    ]);
    let host = print_plugin_host(Some(&runtime)).expect("the runtime builds the print host");
    // The print run's session-context push: the native gate reads this id.
    host.update_session_context(model_snapshot("glm-4.6"));
    let hooks = print_transport_hooks(Some(&host), Some(&runtime));

    let before = hooks
        .before_provider_request
        .as_ref()
        .expect("native interest and the JS handler both install the hook");
    let returned = before(anchored_body()).await;
    // The Rust builtin sanitized in place BEFORE the JS handler ran: the
    // handler's spread saw the sanitized payload, and its result is the
    // merged replacement the transport uses.
    assert_eq!(tool_pattern(&returned), &serde_json::json!("foo"));
    assert_eq!(returned["jsSeen"], serde_json::json!(true));
    // Exactly ONE JS dispatch per emitted request: the host's adapter owns
    // the runtime handle, so a hand-rolled second `runtime.emit` on top would
    // count two here.
    let count = runtime
        .eval_json("JSON.stringify(globalThis.__bprCount)")
        .await
        .expect("probe the JS dispatch counter");
    assert_eq!(count, serde_json::json!(1));

    // The response observer rides the same host route.
    let after = hooks
        .after_provider_response
        .as_ref()
        .expect("the JS observer installs the hook");
    after(
        418,
        std::collections::BTreeMap::from([("x-hdr".to_string(), "y".to_string())]),
    )
    .await;
    let status = runtime
        .eval_json("JSON.stringify(globalThis.__respStatus)")
        .await
        .expect("probe the recorded status");
    assert_eq!(status, serde_json::json!(418));

    // Without a host the fallback still dispatches the JS side (the
    // host-less seam stays intact).
    let fallback = print_transport_hooks(None, Some(&runtime));
    assert!(fallback.before_provider_request.is_some());
}

// ---------------------------------------------------------------------------
// The `message_end_repair` loop seam
// ---------------------------------------------------------------------------

/// The print host's turn cap: `print_loop_deps` starts at the shared
/// non-interactive default (1000, not the old 100), and `run_print`'s
/// `print_apply_max_turns` seam installs `Config::max_turns` (the
/// `PIR_MAX_TURNS`-aware process value) for every prompt run.
#[test]
fn print_turn_cap_defaults_to_1000_and_follows_the_process_config() {
    let mut deps = print_loop_deps(
        Arc::new(ToolRegistry::new()),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        PathBuf::from("."),
        None,
        None,
        None,
        ThinkingLevel::Off,
    );
    assert_eq!(
        deps.max_turns,
        pir::config::defaults::DEFAULT_AGENT_MAX_TURNS,
        "a bare print deps builder carries the non-interactive default"
    );
    assert_eq!(deps.max_turns, 1_000);

    let config = pir::config::Config {
        max_turns: 2_500,
        ..pir::config::Config::default()
    };
    print_apply_max_turns(&mut deps, &config);
    assert_eq!(
        deps.max_turns, 2_500,
        "the process config's value is what a print prompt run uses"
    );
}

/// `print_loop_deps` is the deps builder `run_print` fills; the `tool-repair`
/// builtin's message_end hook rides `LoopDeps::message_end_repair` on the
/// returned deps (print.rs assigns it right after the call). This pins that
/// loop seam for the print path: a hook installed there runs after
/// finalization and its returned message wins over the streamed one.
#[tokio::test(flavor = "current_thread")]
async fn print_loop_deps_carry_the_message_end_repair_seam() {
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let repair: MessageEndRepairFn = Arc::new({
        let seen = Arc::clone(&seen);
        move |context: MessageEndRepairContext| {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                seen.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(context.model.as_ref().map(|model| model.id.clone()));
                let mut message = context.message;
                message.content = vec![ContentBlock::Text(TextContent {
                    text: "repaired".to_owned(),
                    text_signature: None,
                })];
                Some(message)
            })
        }
    });

    let mut deps = print_loop_deps(
        Arc::new(ToolRegistry::new()),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        PathBuf::from("."),
        None,
        None,
        None,
        // This pin is about the repair seam, not reasoning: `off` keeps the
        // scripted run's request shape un-reasoned like before the level arg.
        ThinkingLevel::Off,
    );
    deps.stream = Box::new(|_request: pir::agent::agent_loop::ProviderRequest| {
        Box::pin(async move {
            AssistantMessage {
                api: "fake".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "streamed".to_owned(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                timestamp: 1,
                usage: Usage::default(),
                ..AssistantMessage::default()
            }
        })
    });
    deps.message_end_repair = Some(repair);

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.turns, 1);
    let text = result
        .terminal
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        text, "repaired",
        "the deps repair hook must replace the streamed message"
    );
    assert_eq!(
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [None],
        "the model-less run hands the repair `None` (the strip gates stay off)"
    );
}

// ---------------------------------------------------------------------------
// `background: true` in the print host
// ---------------------------------------------------------------------------
//
// Upstream `pi` has NO background tool-call concept at all (grep: no
// `background` argument, no `BackgroundTool`/`waitForBackground` symbol), so
// there is no upstream behavior to mirror for the one-shot host; its closest
// relative — print-mode.ts's signal path — kills tracked detached shell
// children instead of waiting for them. The print host therefore reuses
// pir's OWN established semantics (`agent_session/background.rs`, pinned
// end-to-end by `tests/suites/background_tool_calls.rs`) and adds the one
// print-only decision, the exit settle:
//
// - `PrintBackground` installs the SAME registry + sink + delivery the
//   session builds: a `background: true` call is detached by preflight's
//   stamp, the batch answers with the scheduler's placeholder, and the real
//   completion lands as a `backgroundToolResult` transcript entry (when the
//   run is persisted), a `SessionEvent::BackgroundToolEnd` + `json` frame,
//   and — while a prompt run is live — a follow-up injection that reaches the
//   running model's next turn boundary;
// - after the last prompt the host waits `DEFAULT_BACKGROUND_EXIT_WAIT` for
//   the calls that outlived it — completion-driven: the wait returns as soon
//   as the last call settles, so the process never leaves a result behind —
//   then ABORTS what is still running: stderr always reports it, `--mode
//   json` emits the abort as a same-shaped `background_tool_end` frame, and
//   text mode exits 1 (a lost result is never silent, and a hung call can
//   never hang the process);
// - a completion whose CALL settled but whose DELIVERY outlived both the
//   budget and the abort grace has nothing left to abort: it is reported
//   through `BackgroundExit::undelivered` (stderr in both modes, text mode
//   exit 1), so even that disposition has no silent arm;
// - the declared tool set now offers the `background` argument
//   (`print_tool_definitions` uses the shared session builder), so a
//   print-mode model can actually ask for the background.

/// A probe tool that records the order of its lifecycle events around a
/// bounded delay (or a release gate), and observes its own abort signal the
/// way a real long-running tool does. The schema is strict
/// (`additionalProperties: false`) and does NOT declare `background`: if the
/// scheduler ever stopped stripping the shared argument, the call would fail
/// validation instead of running, so the recorded-args pin fails loudly.
struct BackgroundProbe {
    order: Arc<Mutex<Vec<&'static str>>>,
    received: Arc<Mutex<Vec<serde_json::Value>>>,
    delay: Duration,
    /// When set, the call parks on this gate instead of sleeping (the exit
    /// settle's "still running" state, released by the test or the abort).
    release: Option<Arc<tokio::sync::Notify>>,
    output: &'static str,
    /// When set, the call completes NORMALLY (not aborted) with a failed
    /// result: the delivery's `isError` branch, which the abort frame must
    /// stay distinguishable from.
    error: bool,
}

impl BackgroundProbe {
    fn new(delay: Duration, output: &'static str) -> Self {
        Self {
            order: Arc::new(Mutex::new(Vec::new())),
            received: Arc::new(Mutex::new(Vec::new())),
            delay,
            release: None,
            output,
            error: false,
        }
    }

    /// A call that never finishes on its own: it parks until explicitly
    /// released or aborted.
    fn held(output: &'static str) -> Self {
        Self {
            order: Arc::new(Mutex::new(Vec::new())),
            received: Arc::new(Mutex::new(Vec::new())),
            delay: Duration::from_secs(0),
            release: Some(Arc::new(tokio::sync::Notify::new())),
            output,
            error: false,
        }
    }

    /// A call that settles by itself with a FAILED result (the delivery's
    /// `isError: true` path, as opposed to the exit-time abort frame).
    fn failing(output: &'static str) -> Self {
        Self {
            order: Arc::new(Mutex::new(Vec::new())),
            received: Arc::new(Mutex::new(Vec::new())),
            delay: Duration::from_millis(50),
            release: None,
            output,
            error: true,
        }
    }

    fn order(&self) -> Arc<Mutex<Vec<&'static str>>> {
        Arc::clone(&self.order)
    }

    fn received(&self) -> Arc<Mutex<Vec<serde_json::Value>>> {
        Arc::clone(&self.received)
    }

    fn release(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(
            self.release
                .as_ref()
                .expect("only a held probe has a release gate"),
        )
    }
}

impl AgentTool for BackgroundProbe {
    fn name(&self) -> &'static str {
        "background-probe"
    }

    fn description(&self) -> &'static str {
        "records entry/exit order around a bounded delay or a release gate"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "integer" } },
            "additionalProperties": false
        }))
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let order = Arc::clone(&self.order);
        let received = Arc::clone(&self.received);
        let args = args.clone();
        let delay = self.delay;
        let release = self.release.clone();
        let output = self.output;
        let error = self.error;
        // The detached call's OWN abort signal (the scheduler installs it on
        // the call's context, `build_detached_call`); a foreground call has
        // none and simply waits for the gate.
        let signal = ctx.signal.clone();
        Box::pin(async move {
            received
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(args);
            order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("call-start");
            let waiting = async move {
                match release {
                    Some(release) => release.notified().await,
                    None => tokio::time::sleep(delay).await,
                }
            };
            tokio::select! {
                _ = waiting => {}
                _ = async {
                    match signal.as_ref() {
                        Some(signal) => signal.cancelled().await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    order
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push("call-aborted");
                    return ToolResult::error("Command aborted");
                }
            }
            order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("call-end");
            if error {
                ToolResult::error(output)
            } else {
                ToolResult::ok(output)
            }
        })
    }
}

/// The print host's background host + the loop deps `run_print` fills for one
/// prompt run: the scripted stream in the `LoopDeps::stream` seam, and the
/// REAL scheduler detach sink + follow-up channel installed through
/// [`PrintBackground::install`] — exactly the production wiring.
///
/// `gate` is the determinism seam of the live-injection pin: when set, the
/// first post-tool request waits until the in-flight background call has
/// STARTED (`call-start` recorded) and the host's registry reports it,
/// records `request-1-in-flight`, releases the probe, waits for the host to
/// drain (delivery complete: event + persistence + follow-up enqueue), and
/// only then returns its terminal message. The frame is therefore queued
/// before the turn's follow-up poll, and the loop must run one more turn
/// carrying it.
fn scripted_background_stream(
    order: Arc<Mutex<Vec<&'static str>>>,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    gate: Option<(Arc<PrintBackground>, Arc<tokio::sync::Notify>)>,
) -> StreamFn {
    Box::new(move |request: ProviderRequest| {
        let order = Arc::clone(&order);
        let requests = Arc::clone(&requests);
        let gate = gate.clone();
        Box::pin(async move {
            let index = {
                let mut captured = requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                captured.push(request.context.messages.clone());
                captured.len() - 1
            };
            if index == 0 {
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "call-bg".into(),
                        name: "background-probe".into(),
                        arguments: BTreeMap::from([
                            ("value".into(), serde_json::json!(7)),
                            ("background".into(), serde_json::json!(true)),
                        ]),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    })],
                    stop_reason: StopReason::ToolUse,
                    timestamp: 1,
                    ..AssistantMessage::default()
                }
            } else {
                if index == 1 {
                    if let Some((background, release)) = gate.as_ref() {
                        let deadline = std::time::Instant::now() + Duration::from_secs(20);
                        loop {
                            let started = order
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .contains(&"call-start");
                            if background.call_count() == 1 && started {
                                break;
                            }
                            assert!(
                                std::time::Instant::now() < deadline,
                                "the background call never reached the in-flight state: count={} order={:?}",
                                background.call_count(),
                                order.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                            );
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        order
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push("request-1-in-flight");
                        // `notify_one` stores a permit when the probe has not
                        // registered its waiter yet (the `call-start` marker
                        // is pushed just before it polls), so the release can
                        // never be lost between the marker and the wait.
                        release.notify_one();
                        assert!(
                            background.wait_for_idle(Duration::from_secs(20)).await,
                            "the released background call never delivered"
                        );
                    }
                }
                order
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(if index == 1 { "request-1" } else { "request-2" });
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "print done".into(),
                        text_signature: None,
                    })],
                    stop_reason: StopReason::Stop,
                    timestamp: 1,
                    ..AssistantMessage::default()
                }
            }
        })
    })
}

/// Build the print host's deps for the one probe tool, with `background`
/// installed (sink + follow-up channel) and the scripted stream in the
/// `LoopDeps::stream` seam. `gate` arms the live-injection determinism seam.
fn print_background_deps(
    probe: Arc<BackgroundProbe>,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    background: Arc<PrintBackground>,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> LoopDeps {
    let order = probe.order();
    let mut registry = ToolRegistry::new();
    registry.register(probe);
    let mut deps = print_loop_deps(
        Arc::new(registry),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        PathBuf::from("."),
        None,
        None,
        None,
        ThinkingLevel::Off,
    );
    // The production installation (`run_print` does exactly this per prompt).
    background.install(&mut deps);
    deps.stream = scripted_background_stream(
        order,
        requests,
        gate.map(|release| (Arc::clone(&background), release)),
    );
    deps
}

/// A store-backed print session (the `SharedSession` shape `run_print`
/// builds), in memory: no temp JSONL files, same `append_custom_message_entry`
/// surface.
fn in_memory_print_session(label: &str) -> SharedSession {
    let cwd = std::env::temp_dir().join(format!("pir-print-bg-{label}-{}", std::process::id()));
    let manager =
        SessionManager::in_memory(&cwd.to_string_lossy(), None).expect("in-memory session");
    Arc::new(tokio::sync::Mutex::new(Some(manager)))
}

/// The `backgroundToolResult` entries the host persisted, in entry order.
async fn stored_background_results(session: &SharedSession) -> Vec<CustomMessageEntry> {
    let guard = session.lock().await;
    let Some(manager) = guard.as_ref() else {
        return Vec::new();
    };
    manager
        .entries()
        .into_iter()
        .filter_map(|entry| match entry {
            FileEntry::CustomMessage(entry) if entry.custom_type == "backgroundToolResult" => {
                Some(entry)
            }
            _ => None,
        })
        .collect()
}

/// Run one prompt against the given probe/host and settle the host afterwards
/// (the `run_print` sequence: begin_run → loop → end_run → settle).
async fn run_print_background_prompt(
    probe: Arc<BackgroundProbe>,
    background: Arc<PrintBackground>,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> (
    pir::agent::agent_loop::LoopResult,
    Arc<Mutex<Vec<Vec<Message>>>>,
) {
    let requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let deps = print_background_deps(probe, Arc::clone(&requests), Arc::clone(&background), gate);
    background.begin_run();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        run_with_options(deps, Context::default(), None, LoopOptions::default()),
    )
    .await
    .expect("the print run must settle inside the bounded wait");
    background.end_run();
    (result, requests)
}

/// Case 1 — the full live path on every delivery surface. The model asks for
/// the background on a HELD call and keeps turning: the batch answers with the
/// scheduler's placeholder immediately, the loop reaches its next provider
/// request while the call is still in flight, and the host's registry owns it.
/// The gate then releases the call and waits for the host to drain, so the
/// completion's follow-up frame is queued before the terminal turn's
/// follow-up poll: the loop runs one more turn carrying it — the print-mode
/// counterpart of the session's
/// `nested_background_via_the_real_session_sink_persists_and_reports`.
///
/// Pins: the placeholder is the only provider tool_result (the real result
/// never arrives as one), the frame reaches the live model, and the host's
/// `background_tool_end` event, text frame and `backgroundToolResult`
/// transcript entry all carry the real output; the exit settle drains clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // the recorded-request guard is dropped before every await
async fn print_background_call_is_detached_and_delivered_on_every_surface() {
    let probe = Arc::new(BackgroundProbe::held("probe-output"));
    let order = probe.order();
    let received = probe.received();
    let release = probe.release();
    let session = in_memory_print_session("detached");
    let background = PrintBackground::new(Arc::clone(&session), None);
    let (result, requests) =
        run_print_background_prompt(Arc::clone(&probe), Arc::clone(&background), Some(release))
            .await;

    // Tool-call turn, the gated final turn, then the follow-up turn the
    // injected completion forced.
    assert_eq!(
        result.turns, 3,
        "tool call, final turn, injected-frame turn"
    );
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);
    assert!(result
        .terminal
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Text(text) if text.text == "print done")));

    // Detachment ordering: the loop built request 1 WHILE the call ran (the
    // gate only releases it after `request-1-in-flight`), and the call ended
    // only after that release.
    let order = order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        order.as_slice(),
        [
            "call-start",
            "request-1-in-flight",
            "call-end",
            "request-1",
            "request-2"
        ],
        "the loop must not await the detached call"
    );

    // The tool never saw the scheduler-owned argument (its strict schema
    // would have rejected the call if the key were still present).
    let received = received
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(received.len(), 1, "the probe must run exactly once");
    assert!(
        received[0].get("background").is_none(),
        "the argument must be stripped: {}",
        received[0]
    );
    drop(received);

    // The batch answered with the scheduler's placeholder, exactly once: the
    // real result never comes back as a provider tool_result (the pairing
    // contract), and the placeholder carries the scheduler's details marker.
    assert_eq!(result.tool_results.len(), 1);
    let placeholder = &result.tool_results[0];
    assert_eq!(placeholder.tool_call_id, "call-bg");
    assert_eq!(placeholder.tool_name, "background-probe");
    assert!(
        placeholder
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(text)
                if text.text.contains("is running in the background (callId: call-bg)"))),
        "the placeholder must be the batch's answer: {:?}",
        placeholder.content
    );
    assert_eq!(
        placeholder
            .details
            .as_ref()
            .and_then(|details| details.get("background")),
        Some(&serde_json::Value::Bool(true)),
        "the placeholder details must mark the detach"
    );
    assert_eq!(
        result
            .new_messages
            .iter()
            .filter(|message| matches!(message, Message::ToolResult(_)))
            .count(),
        1,
        "exactly one provider tool_result (the placeholder): {:?}",
        result.new_messages
    );

    // The live model-facing delivery: request 1 was built before the call
    // settled (placeholder only — no completion frame), and the injected
    // follow-up frame reached the loop's NEXT request.
    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        requests.len(),
        3,
        "tool-call turn, gated final turn, follow-up turn"
    );
    let second = serde_json::to_string(&requests[1]).expect("serialize request 1");
    assert!(
        second.contains("is running in the background"),
        "request 1 must carry the placeholder: {second}"
    );
    assert!(
        !second.contains("background-tool-result"),
        "the completion had not settled when request 1 was built: {second}"
    );
    let third = serde_json::to_string(&requests[2]).expect("serialize request 2");
    assert!(
        third.contains("background-tool-result") && third.contains("probe-output"),
        "request 2 must carry the injected completion frame: {third}"
    );
    drop(requests);

    // The host's event record: one `background_tool_end` in the RPC wire
    // shape (the same mapping `--mode json` streams).
    let reports = background.reports();
    assert_eq!(reports.len(), 1, "reports: {reports:?}");
    let json = pir::agent_session::session_event_to_json(&reports[0]);
    assert_eq!(json["type"], "background_tool_end");
    assert_eq!(json["callId"], "call-bg");
    assert_eq!(json["toolName"], "background-probe");
    assert_eq!(json["isError"], serde_json::Value::Bool(false));
    assert_eq!(json["output"], "probe-output");
    assert!(json["durationMs"].is_u64(), "json: {json}");

    // The text surface: the final answer first, then the completion frame
    // verbatim (`text_with_background_reports`, what `run_print` returns).
    let stdout_payload = text_with_background_reports("print done", &reports);
    assert!(
        stdout_payload.starts_with(
            "print done\n\n<background-tool-result tool=\"background-probe\" callId=\"call-bg\" status=\"ok\""
        ),
        "{stdout_payload}"
    );
    assert!(
        stdout_payload.contains("\nprobe-output\n</background-tool-result>"),
        "{stdout_payload}"
    );

    // The transcript entry: the framed `backgroundToolResult` custom message,
    // details in the upstream shape, written by the real delivery.
    let stored = stored_background_results(&session).await;
    assert_eq!(stored.len(), 1, "stored: {stored:?}");
    let entry = &stored[0];
    assert!(!entry.display, "display:false like the session's entry");
    let frame = entry.content.as_str().expect("the frame is a string");
    assert!(
        frame.contains("call-bg") && frame.contains("probe-output"),
        "{frame}"
    );
    let details = entry.details.as_ref().expect("entry details");
    assert_eq!(details["callId"], "call-bg");
    assert_eq!(details["toolName"], "background-probe");
    assert_eq!(details["status"], "ok");
    assert_eq!(details["details"], serde_json::json!({}));

    // The exit settle: nothing left in flight, no abort, exit 0.
    let exit = background.settle(Duration::from_secs(2)).await;
    assert!(!exit.timed_out, "the host must have drained: {exit:?}");
    assert!(exit.aborted.is_empty());
    assert_eq!(exit.exit_code(), 0);
    assert_eq!(background.call_count(), 0);
}

/// Case 2 — a call still running when the last prompt settles: the exit
/// semantics. Upstream defines nothing here (no background calls exist), so
/// the print host's conservative rule applies: bounded wait → abort → explicit
/// diagnostics. The probe parks (its own abort signal observed through
/// `ToolContext::signal`), the settle's bounded wait expires, the host aborts
/// it, and the abort reaches the call inside the grace before `settle`
/// returns.
///
/// Pins: the wait is actually bounded (not an early return), the disposition
/// is `timed_out` with the call's identity and text-mode exit code 1, the
/// aborted call delivers NOTHING (the session/RPC abort parity), and the
/// abort is reported through the same-shaped `background_tool_end` frame and
/// the stderr line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_background_call_still_running_at_exit_is_bounded_waited_then_aborted() {
    let probe = Arc::new(BackgroundProbe::new(
        Duration::from_secs(60),
        "never-delivered",
    ));
    let order = probe.order();
    let session = in_memory_print_session("timeout");
    let background = PrintBackground::new(Arc::clone(&session), None);
    let (result, _requests) =
        run_print_background_prompt(Arc::clone(&probe), Arc::clone(&background), None).await;

    // The loop reached its final message while the 60s call was still in
    // flight: the detach is real, and the host owns the call at exit.
    assert_eq!(result.turns, 2, "tool-call turn then the final turn");
    assert_eq!(
        background.call_count(),
        1,
        "the unreleased call must still be in flight at exit"
    );

    let budget = Duration::from_millis(150);
    let started = std::time::Instant::now();
    let exit = background.settle(budget).await;
    assert!(
        started.elapsed() >= budget,
        "the settle must wait the FULL bounded budget before aborting (waited {:?})",
        started.elapsed()
    );
    assert!(exit.timed_out, "the call was still running: {exit:?}");
    assert_eq!(
        exit.exit_code(),
        1,
        "text mode exits nonzero on a lost result"
    );
    assert_eq!(exit.aborted.len(), 1, "aborted: {:?}", exit.aborted);
    let aborted = &exit.aborted[0];
    assert_eq!(aborted.call_id, "call-bg");
    assert_eq!(aborted.tool_name, "background-probe");
    assert!(aborted.parent_call_id.is_none(), "a model-issued call");

    // The abort reached the call inside the grace window (`call-aborted` is
    // pushed by the probe's own cancellation branch), so the process does not
    // just drop a running task.
    let order = order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(
        order.contains(&"call-aborted"),
        "the probe must observe its abort: {order:?}"
    );
    assert_eq!(background.call_count(), 0, "the registry drained");

    // Abort parity with the session/RPC surface: an aborted call delivers
    // NOTHING — no event, no text frame, no transcript entry.
    assert!(
        background.reports().is_empty(),
        "an aborted call must not be reported as a completion: {:?}",
        background.reports()
    );
    assert!(stored_background_results(&session).await.is_empty());
    assert!(
        !text_with_background_reports("print done", &background.reports())
            .contains("background-tool-result")
    );

    // The explicit diagnostics: the json abort frame keeps the completion
    // event's shape (`background_tool_end`) but marks the abort, and the
    // stderr line names the call and the reason.
    let reason = "still running 150 ms after the last prompt finished";
    let json = pir::agent_session::session_event_to_json(&background_abort_event(aborted, reason));
    assert_eq!(json["type"], "background_tool_end");
    assert_eq!(json["callId"], "call-bg");
    assert_eq!(json["toolName"], "background-probe");
    assert_eq!(json["isError"], serde_json::Value::Bool(true));
    assert_eq!(json["details"]["aborted"], serde_json::Value::Bool(true));
    assert_eq!(json["details"]["reason"], reason);
    assert!(
        json["output"]
            .as_str()
            .expect("abort output")
            .contains(reason),
        "{json}"
    );
    let line = background_abort_diagnostic(aborted, reason);
    assert!(
        line.contains("background-probe") && line.contains("call-bg") && line.contains(reason),
        "{line}"
    );
}

/// Case 3 — `--no-session`: the store-less print host. The transcript has no
/// place for the `backgroundToolResult` entry, so the completion's delivery is
/// the live follow-up injection plus the event/text surfaces. Same gated run
/// as case 1, over a `SharedSession` holding `None` (exactly what
/// `--no-session` builds).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // the recorded-request guard is dropped before every await
async fn print_background_without_session_delivers_live_and_on_stdout() {
    let probe = Arc::new(BackgroundProbe::held("no-session-output"));
    let release = probe.release();
    let session: SharedSession = Arc::new(tokio::sync::Mutex::new(None));
    let background = PrintBackground::new(Arc::clone(&session), None);
    let (result, requests) =
        run_print_background_prompt(Arc::clone(&probe), Arc::clone(&background), Some(release))
            .await;

    assert_eq!(result.turns, 3, "the injected frame forced one more turn");
    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let third = serde_json::to_string(&requests[2]).expect("serialize request 2");
    assert!(
        third.contains("background-tool-result") && third.contains("no-session-output"),
        "a store-less host must still inject the completion into the live run: {third}"
    );
    drop(requests);

    let reports = background.reports();
    assert_eq!(reports.len(), 1, "reports: {reports:?}");
    let payload = text_with_background_reports("print done", &reports);
    assert!(
        payload.contains("no-session-output") && payload.contains("call-bg"),
        "the stdout payload is the store-less delivery surface: {payload}"
    );
    assert!(
        stored_background_results(&session).await.is_empty(),
        "an ephemeral run persists nothing"
    );
    let exit = background.settle(Duration::from_secs(2)).await;
    assert!(!exit.timed_out, "the host must have drained: {exit:?}");
    assert_eq!(exit.exit_code(), 0);
}

/// Case 4 — the POSITIVE half of the exit rule (the production `run_print`
/// sequence: the last prompt settles, then the host awaits
/// `background.settle(DEFAULT_BACKGROUND_EXIT_WAIT)`): the process must NOT
/// leave while a background call is in flight, and must leave as soon as the
/// call SETTLES — the wait is completion-driven, not a fixed sleep. Case 1
/// only ever settles an already-drained registry and case 2 only covers the
/// expiry-and-abort half, so neither pins this direction.
///
/// The held probe is still in flight when the prompt is over. The test starts
/// the exit settle, proves it is STILL WAITING after a delay (the process
/// would not have exited), then releases the call and proves the settle
/// returns promptly with the completion delivered in full — the "exit once
/// they settle" clause, plus the delivery surfaces reaching the late result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_background_exit_waits_while_in_flight_and_returns_on_settle() {
    let probe = Arc::new(BackgroundProbe::held("late-output"));
    let release = probe.release();
    let session = in_memory_print_session("exit-wait");
    let background = PrintBackground::new(Arc::clone(&session), None);
    let (result, _requests) =
        run_print_background_prompt(Arc::clone(&probe), Arc::clone(&background), None).await;

    // The prompt is over; the call is still in flight and the host owns it.
    assert_eq!(result.turns, 2, "tool-call turn then the final turn");
    // Order/pairing at the start of the lifecycle: the batch answered with
    // exactly one placeholder tool_result (the real result must never arrive
    // as a second one), and the completion EVENT has not happened yet.
    assert_eq!(result.tool_results.len(), 1, "the batch placeholder");
    assert_eq!(result.tool_results[0].tool_call_id, "call-bg");
    assert_eq!(
        background.call_count(),
        1,
        "the held call must still be in flight when the last prompt settles"
    );
    assert!(
        background.reports().is_empty(),
        "nothing may be delivered before the call settles"
    );

    // The exit settle (`run_print`'s last await) must not return while the
    // call is in flight: after a real delay it is still waiting.
    let settle = tokio::spawn({
        let background = Arc::clone(&background);
        async move { background.settle(Duration::from_secs(30)).await }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !settle.is_finished(),
        "the exit wait returned while a background call was in flight"
    );

    // Releasing the call must wake the wait immediately: the host exits as
    // soon as the call settles, delivering it in full.
    release.notify_one();
    let exit = tokio::time::timeout(Duration::from_secs(10), settle)
        .await
        .expect("the settle must return once the in-flight call settles")
        .expect("the settle task must not panic");
    assert!(
        !exit.timed_out,
        "the call settled inside the budget: {exit:?}"
    );
    assert!(exit.aborted.is_empty());
    assert_eq!(exit.exit_code(), 0);
    assert_eq!(background.call_count(), 0, "the registry drained");

    let reports = background.reports();
    assert_eq!(
        reports.len(),
        1,
        "the completion of a call that outlived the prompt is delivered: {reports:?}"
    );
    match &reports[0] {
        pir::agent_session::SessionEvent::BackgroundToolEnd {
            call_id, output, ..
        } => {
            assert_eq!(call_id, "call-bg");
            assert_eq!(output, "late-output");
        }
        other => panic!("expected a background_tool_end, got {other:?}"),
    }
    let stored = stored_background_results(&session).await;
    assert_eq!(
        stored.len(),
        1,
        "the late completion is persisted: {stored:?}"
    );
}

/// Case 5 — `PrintBackground::abort_in_flight` (the terminal-error teardown
/// `run_print` performs when a text-mode run ends with an error stop reason):
/// the headless `abort_all_background` twin of the print host. Every in-flight
/// call is aborted through its OWN signal, is reported back for the caller's
/// diagnostics, and delivers NOTHING — the same abort semantics as the session
/// and RPC surfaces (`background_tool_calls`, `rpc_commands`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_background_abort_in_flight_cancels_and_delivers_nothing() {
    let probe = Arc::new(BackgroundProbe::held("never-delivered"));
    let order = probe.order();
    let session = in_memory_print_session("abort-all");
    let background = PrintBackground::new(Arc::clone(&session), None);
    let (result, _requests) =
        run_print_background_prompt(Arc::clone(&probe), Arc::clone(&background), None).await;
    assert_eq!(result.turns, 2);
    assert_eq!(background.call_count(), 1, "the call must be in flight");

    // The teardown path: abort every in-flight call without waiting for a
    // completion (the run failed and its caller gets an error, not a payload).
    let aborted = background.abort_in_flight().await;
    assert_eq!(aborted.len(), 1, "aborted: {aborted:?}");
    assert_eq!(aborted[0].call_id, "call-bg");
    assert_eq!(aborted[0].tool_name, "background-probe");
    assert_eq!(background.call_count(), 0, "the registry drained");

    // The abort reached the call through its OWN signal (the probe observes
    // it), so nothing is left dangling for the process exit.
    let order = order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(
        order.contains(&"call-aborted"),
        "the probe must observe the headless abort: {order:?}"
    );

    // Abort parity: an aborted call delivers nothing on any surface, and the
    // host is drained (a following settle cannot time out).
    assert!(
        background.reports().is_empty(),
        "an aborted call must not be reported: {:?}",
        background.reports()
    );
    assert!(stored_background_results(&session).await.is_empty());
    assert!(background.wait_for_idle(Duration::from_secs(2)).await);
    let exit = background.settle(Duration::from_secs(2)).await;
    assert!(
        !exit.timed_out,
        "the teardown already aborted everything: {exit:?}"
    );
}

/// Case 6 — a background call that completes NORMALLY with a FAILED result:
/// the delivery's `isError: true` path (distinct from the exit-time abort
/// frame, which also reports `isError: true` but marks `details.aborted`). A
/// headless consumer must be able to tell a failed call from an aborted one
/// from the event/frame/transcript alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_background_failure_is_delivered_as_an_error_completion() {
    let probe = Arc::new(BackgroundProbe::failing("boom"));
    let session = in_memory_print_session("failure");
    let background = PrintBackground::new(Arc::clone(&session), None);
    let (result, _requests) =
        run_print_background_prompt(Arc::clone(&probe), Arc::clone(&background), None).await;
    // Two turns, or three when the 50 ms failure won the race against the
    // loop's final follow-up poll (the frame is then injected into one more
    // turn). Both are correct run shapes; the delivery assertions below are
    // what this case pins.
    assert!(result.turns >= 2, "turns: {}", result.turns);

    // The call settles on its own inside the exit wait (no abort).
    let exit = background.settle(Duration::from_secs(10)).await;
    assert!(
        !exit.timed_out,
        "the failure settled inside the budget: {exit:?}"
    );
    assert!(exit.aborted.is_empty());

    // The event: `background_tool_end` with `isError: true`, the failed
    // output, and NO `aborted` marker (the abort frame's discriminator).
    let reports = background.reports();
    assert_eq!(reports.len(), 1, "reports: {reports:?}");
    let json = pir::agent_session::session_event_to_json(&reports[0]);
    assert_eq!(json["type"], "background_tool_end");
    assert_eq!(json["callId"], "call-bg");
    assert_eq!(json["toolName"], "background-probe");
    assert_eq!(json["isError"], serde_json::Value::Bool(true));
    assert_eq!(json["output"], "boom");
    assert!(
        json.get("details")
            .and_then(|details| details.get("aborted"))
            .is_none(),
        "a failed completion is not an abort: {json}"
    );

    // The text surface: the frame carries `status="error"`.
    let payload = text_with_background_reports("print done", &reports);
    assert!(
        payload.contains(
            "<background-tool-result tool=\"background-probe\" callId=\"call-bg\" status=\"error\""
        ),
        "{payload}"
    );
    assert!(
        payload.contains("\nboom\n</background-tool-result>"),
        "{payload}"
    );

    // The transcript entry stores the same error status.
    let stored = stored_background_results(&session).await;
    assert_eq!(stored.len(), 1, "stored: {stored:?}");
    let details = stored[0].details.as_ref().expect("entry details");
    assert_eq!(details["status"], "error");
    assert_eq!(details["callId"], "call-bg");
}

/// Case 7 — the last honesty check of the exit rule: a completion whose CALL
/// already settled but whose DELIVERY (the persistence leg into the shared
/// session) is still running when both the budget and the abort grace expire.
/// Nothing is left to abort, so the settle reports it through
/// `BackgroundExit::undelivered` (text mode exits 1; `run_print` warns on
/// stderr in both modes) instead of exiting silently with a half-written
/// result.
///
/// The test holds the shared session lock while the probe is released: the
/// delivery emits its event (already visible to a `--mode json` consumer,
/// since the event precedes persistence) and then parks on the lock — exactly
/// the stuck-delivery shape, without any timing guesswork.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_background_settle_reports_a_delivery_that_outlived_the_grace() {
    let probe = Arc::new(BackgroundProbe::held("stuck-output"));
    let release = probe.release();
    let session = in_memory_print_session("undelivered");
    let background = PrintBackground::new(Arc::clone(&session), None);
    let (result, _requests) =
        run_print_background_prompt(Arc::clone(&probe), Arc::clone(&background), None).await;
    assert_eq!(result.turns, 2, "tool-call turn then the final turn");
    assert_eq!(background.call_count(), 1);

    // Block the delivery's persistence leg, then settle the call: the entry
    // leaves the registry and the event fires, but the completion task parks
    // on the session lock before it can persist.
    let guard = session.lock().await;
    release.notify_one();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while background.call_count() != 0 || background.reports().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the released call's delivery never started"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let exit = background.settle(Duration::from_millis(150)).await;
    assert!(!exit.timed_out, "the call itself settled: {exit:?}");
    assert!(
        exit.aborted.is_empty(),
        "nothing was left to abort: {exit:?}"
    );
    assert_eq!(
        exit.undelivered, 1,
        "the still-running delivery must be reported, never exit silently: {exit:?}"
    );
    assert_eq!(
        exit.exit_code(),
        1,
        "text mode exits nonzero on a partial exit"
    );

    // Releasing the lock lets the delivery finish normally (nothing was
    // cancelled), so the transcript entry lands after the settle returned.
    drop(guard);
    assert!(
        background.wait_for_idle(Duration::from_secs(5)).await,
        "the delivery must finish once the lock is released"
    );
    assert_eq!(background.call_count(), 0);
    let stored = stored_background_results(&session).await;
    assert_eq!(stored.len(), 1, "the entry lands once persistence can run");
}

/// The provider-visible half of the feature: `run_print` declares its tools
/// through [`print_tool_definitions`], the shared session builder — so a
/// print-mode model actually learns the scheduler-owned `background` argument
/// (the argument the print scheduler then strips before validation), exactly
/// like the interactive session's declared set. The bare core builder the
/// print host used before this change declared no `background`, which is why
/// no production print run could ask for a background call even once the host
/// supported one.
#[test]
fn print_tool_schemas_offer_the_background_argument() {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(BackgroundProbe::new(
        Duration::from_millis(0),
        "schema-probe",
    )));
    let definitions = print_tool_definitions(&registry);
    assert_eq!(definitions.len(), 1);
    let background = definitions[0].parameters["properties"]
        .get("background")
        .unwrap_or_else(|| {
            panic!(
                "the print declared set must offer the argument: {}",
                definitions[0].parameters
            )
        });
    assert_eq!(background["type"], "boolean");
    // The interactive declared set uses the same builder: identical schemas,
    // including the strict-sampling preference the shared builder sets.
    let declared: Vec<Arc<dyn AgentTool>> = registry.iter().cloned().collect();
    let session_definitions = pir::agent::tool_execution::declared_tool_definitions(
        &declared,
        &BTreeMap::new(),
        &BTreeSet::new(),
    );
    assert_eq!(
        session_definitions[0].parameters, definitions[0].parameters,
        "print declares exactly the session's schema"
    );
    assert_eq!(
        session_definitions[0].constrained_sampling, definitions[0].constrained_sampling,
        "print declares exactly the session's sampling hint"
    );
    // The contrast the change removed: the bare registry builder offers no
    // `background` (and no sampling hint) — nothing else in print uses it.
    let bare = pir::coding_agent::tools::tool_definitions(&registry);
    assert!(
        bare[0].parameters["properties"].get("background").is_none(),
        "the bare builder is the old surface: {}",
        bare[0].parameters
    );
}

// ---------------------------------------------------------------------------
// The resolved starting thinking level on the print request
// ---------------------------------------------------------------------------
//
// Upstream's non-interactive host resolves a fresh session's starting level in
// `createAgentSession` (sdk.ts:250-278): an explicit CLI/model level wins,
// then the per-model `modelThinkingLevels` override, then
// `settings.defaultThinkingLevel` (`off` absent), then the shared
// `DEFAULT_THINKING_LEVEL`. The TUI host applies that same fallback through
// `initial_interactive_thinking` (settings → shared default, `off` absent).
// The print host resolved a level through `find_initial_model` but never put
// it on `LoopDeps`: the field stayed at its `off` default, and
// `registry_stream*` REPLACED the `reasoning` baked into the stream options
// with `off` whenever the two differed — so a thinking-capable model received
// no reasoning field at all. The pins below drive the production deps
// assembly (`print_loop_deps`) into the real registry stream path and assert
// what the provider receives.

/// The reported `ar/deepseek-v4.1-flash` shape: an `openai-completions`
/// reasoning model whose `thinkingLevelMap` nulls `minimal`/`medium`/`xhigh`
/// and names `low`/`high`/`max`.
fn thinking_level_map_model(provider: &str) -> Model {
    let mut model = test_model();
    model.id = "deepseek-v4.1-flash".into();
    model.name = "DeepSeek v4.1 Flash".into();
    model.api = "openai-completions".into();
    model.provider = provider.into();
    model.reasoning = true;
    model.thinking_level_map = Some(BTreeMap::from([
        ("minimal".to_owned(), None),
        ("low".to_owned(), Some("low".to_owned())),
        ("medium".to_owned(), None),
        ("high".to_owned(), Some("high".to_owned())),
        ("xhigh".to_owned(), None),
        ("max".to_owned(), Some("max".to_owned())),
    ]));
    model
}

/// Records the `reasoning` level of every provider request and completes with
/// a plain stop (one-turn run).
struct ReasoningRecorder {
    seen: Arc<Mutex<Vec<Option<ThinkingLevel>>>>,
    model: Model,
}

impl Provider for ReasoningRecorder {
    // `Provider::id` is the trait method: this double reports the model's
    // provider id, which is not `Model::id` (the model name).
    #[allow(clippy::misnamed_getters)]
    fn id(&self) -> &str {
        &self.model.provider
    }

    fn name(&self) -> &str {
        "Reasoning recorder"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new(
            "reasoning recorder key",
            vec!["PIR_TEST_REASONING_KEY".into()],
        )]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(options.reasoning);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: self.model.api.clone(),
                provider: self.model.provider.clone(),
                model: self.model.id.clone(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "ok".to_owned(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                timestamp: 1,
                ..AssistantMessage::default()
            },
        ));
        stream
    }
}

/// Run ONE print-shaped turn at `level` through the production deps assembly
/// and return the reasoning level each provider request carried.
async fn request_levels_for(model: &Model, level: ThinkingLevel) -> Vec<Option<ThinkingLevel>> {
    let seen: Arc<Mutex<Vec<Option<ThinkingLevel>>>> = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(ReasoningRecorder {
        seen: Arc::clone(&seen),
        model: model.clone(),
    }));
    let deps = print_loop_deps(
        Arc::new(ToolRegistry::new()),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        PathBuf::from("."),
        None,
        None,
        None,
        level,
    )
    .with_provider_unchecked(
        Arc::new(registry),
        model.clone(),
        StreamOptions {
            reasoning: Some(level),
            ..StreamOptions::default()
        },
    );
    assert_eq!(
        deps.thinking_level, level,
        "the print deps must carry the resolved level (the loop's request level)"
    );
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(
        result.turns, 1,
        "the scripted stop ends the run in one turn"
    );
    let levels = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(!levels.is_empty(), "the run must have reached the provider");
    levels
}

/// `settings.defaultThinkingLevel` reaches the print request: with `max` the
/// provider sees the mapped `reasoning_effort: "max"`. Before the deps
/// wiring, the same run sent NO reasoning field (the loop replaced the baked
/// `max` with its `off` default, and the map has no `off` entry to emit).
#[tokio::test(flavor = "current_thread")]
async fn print_request_carries_the_settings_default_thinking_level() {
    use pir::agent_session::resolve_starting_thinking_level as resolve;
    use pir::ai::providers::resolution::DEFAULT_THINKING_LEVEL;

    // The shared chain: explicit wins, then per-model, then settings, then
    // the shared default; `off` in the settings default is absent.
    assert_eq!(
        resolve(
            Some(ThinkingLevel::High),
            Some(ThinkingLevel::Low),
            Some(ThinkingLevel::Max)
        ),
        ThinkingLevel::High
    );
    assert_eq!(
        resolve(None, Some(ThinkingLevel::Low), Some(ThinkingLevel::Max)),
        ThinkingLevel::Low
    );
    assert_eq!(
        resolve(None, None, Some(ThinkingLevel::Max)),
        ThinkingLevel::Max
    );
    assert_eq!(
        resolve(None, None, Some(ThinkingLevel::Off)),
        DEFAULT_THINKING_LEVEL
    );
    assert_eq!(resolve(None, None, None), DEFAULT_THINKING_LEVEL);

    let model = thinking_level_map_model("reasoning-max");
    let level = resolve(None, None, Some(ThinkingLevel::Max));
    let levels = request_levels_for(&model, level).await;
    assert_eq!(
        levels,
        [Some(ThinkingLevel::Max)],
        "the provider must see the resolved starting level, not `off`"
    );
    // The wire field, through the adapter's own mapper: `max` -> "max".
    let mut body = serde_json::json!({});
    pir::ai::api::openai::apply_reasoning_effort(&mut body, &model, levels[0]);
    assert_eq!(body["reasoning_effort"], "max");
}

/// Unset and explicit-`off` settings defaults fall back to the shared
/// `DEFAULT_THINKING_LEVEL`, and that level STILL reaches the request even
/// though the model's `thinkingLevelMap` nulls `medium`: the map's `null`
/// means "unsupported for clamping", while the wire mapping is
/// `thinkingLevelMap[level] ?? level`, so `reasoning_effort: "medium"` is
/// sent. Before the deps wiring this request carried no reasoning field at
/// all.
#[tokio::test(flavor = "current_thread")]
async fn print_request_carries_the_shared_default_when_settings_default_is_off_or_unset() {
    use pir::agent_session::resolve_starting_thinking_level as resolve;
    use pir::ai::providers::resolution::DEFAULT_THINKING_LEVEL;

    let model = thinking_level_map_model("reasoning-default");
    for settings_default in [None, Some(ThinkingLevel::Off)] {
        let level = resolve(None, None, settings_default);
        assert_eq!(level, DEFAULT_THINKING_LEVEL);
        let levels = request_levels_for(&model, level).await;
        assert_eq!(
            levels,
            [Some(level)],
            "the shared default must be the request level, not `off`"
        );
        let mut body = serde_json::json!({});
        pir::ai::api::openai::apply_reasoning_effort(&mut body, &model, levels[0]);
        assert_eq!(
            body["reasoning_effort"], "medium",
            "a nulled map entry falls back to the wire key, it does not drop the field"
        );
    }
}

// ---------------------------------------------------------------------------
// Threshold-compaction lifecycle over `--mode json`
// (`compaction_start` / `compaction_end`)
// ---------------------------------------------------------------------------

/// The summarizer double: every compaction `complete` call completes with
/// this text, counted so a run can prove the pipeline actually ran.
struct CompactionSummaryProvider {
    model: Model,
    calls: Arc<Mutex<usize>>,
}

impl Provider for CompactionSummaryProvider {
    #[allow(clippy::misnamed_getters)]
    fn id(&self) -> &str {
        &self.model.provider
    }

    fn name(&self) -> &str {
        "compaction summary double"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
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
        *self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: self.model.api.clone(),
                provider: self.model.provider.clone(),
                model: self.model.id.clone(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "COMPACTION SUMMARY".to_owned(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                usage: Usage {
                    total_tokens: 5,
                    ..Usage::default()
                },
                timestamp: 1,
                ..AssistantMessage::default()
            },
        ));
        stream
    }
}

/// The conversation script of the compaction probes: the FIRST request answers
/// with a tool call whose assistant message records `usage_tokens` (the usage
/// the next turn's threshold check reads), every later request stops. Two
/// requests therefore run: the tool-call turn, then the terminal turn.
fn scripted_compaction_stream(
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    usage_tokens: u64,
) -> StreamFn {
    Box::new(move |request: ProviderRequest| {
        let requests = Arc::clone(&requests);
        Box::pin(async move {
            let index = {
                let mut captured = requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                captured.push(request.context.messages.clone());
                captured.len() - 1
            };
            if index == 0 {
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "call-compaction".into(),
                        name: "background-probe".into(),
                        arguments: BTreeMap::from([("value".into(), serde_json::json!(1))]),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    })],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input: usage_tokens,
                        total_tokens: usage_tokens,
                        ..Usage::default()
                    },
                    timestamp: 1,
                    ..AssistantMessage::default()
                }
            } else {
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "print done".into(),
                        text_signature: None,
                    })],
                    stop_reason: StopReason::Stop,
                    timestamp: 1,
                    ..AssistantMessage::default()
                }
            }
        })
    })
}

/// The JSON-mode sink of the probe: each frame is handed to the PRODUCTION
/// writer [`pir::coding_agent::json::print_session_event`] (exactly what
/// `json_session_event_sink` installs) and recorded as the wire object
/// [`to_session_event_json`] builds, the function that writer serializes, so
/// asserting on the log asserts the `--mode json` stream. The writer's stdout
/// lines are captured by libtest.
fn recording_session_event_sink(
    frames: Arc<Mutex<Vec<serde_json::Value>>>,
) -> PrintSessionEventSink {
    Arc::new(move |event: &SessionEvent| {
        pir::coding_agent::json::print_session_event(event);
        frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(to_session_event_json(event));
    })
}

/// Every `compaction` entry persisted in the probe session.
async fn stored_compactions(session: &SharedSession) -> usize {
    let guard = session.lock().await;
    guard
        .as_ref()
        .map(|manager| {
            manager
                .entries()
                .iter()
                .filter(|entry| matches!(entry, FileEntry::Compaction(_)))
                .count()
        })
        .unwrap_or(0)
}

/// The text blocks of one message, joined (assertions only).
fn message_text(message: &Message) -> String {
    let content = match message {
        Message::User(user) | Message::System(user) | Message::Developer(user) => &user.content,
        Message::Assistant(assistant) => &assistant.content,
        Message::ToolResult(result) => &result.content,
    };
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run the two-request script through the PRODUCTION print compaction hook
/// (resident path) with a real session and the recording JSON sink: the tool
/// call turn records `usage_tokens`, the threshold is checked again on the
/// second turn. Returns the loop result, the recorded frames, the request
/// contexts, and the session.
async fn run_print_compaction_probe(
    usage_tokens: u64,
) -> (
    pir::agent::agent_loop::LoopResult,
    Vec<serde_json::Value>,
    Vec<Vec<Message>>,
    SharedSession,
) {
    let frames: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let session = in_memory_print_session("compaction-events");
    let calls = Arc::new(Mutex::new(0usize));
    let registry = {
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(CompactionSummaryProvider {
            model: test_model(),
            calls: Arc::clone(&calls),
        }));
        Arc::new(registry)
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BackgroundProbe::new(
        Duration::ZERO,
        "tool output",
    )));
    let mut deps = print_loop_deps(
        Arc::new(tools),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        PathBuf::from("."),
        None,
        None,
        None,
        ThinkingLevel::Off,
    );
    deps.stream = scripted_compaction_stream(Arc::clone(&requests), usage_tokens);
    // `test_model().context_window` is 128_000: with a 1_000-token reserve the
    // gate sits at 127_000 tokens, and `keep_recent_tokens: 1` keeps the cut
    // plan trivial (the user message is summarized, the tool turn is kept).
    let hook = print_compaction_hook(
        CompactionConfig {
            enabled: true,
            reserve_tokens: 1_000,
            keep_recent_tokens: 1,
        },
        test_model(),
        registry,
        None,
        ThinkingLevel::Off,
        pir::ai::types::ProviderStreamHooks::default(),
        Arc::clone(&session),
        None,
        None,
        Some(recording_session_event_sink(Arc::clone(&frames))),
    );
    let result = run_with_options(
        deps,
        Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: "compaction probe prompt".to_owned(),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                timestamp: 1,
                sections: None,
            })],
            tools: None,
        },
        None,
        LoopOptions {
            compaction: Some(hook),
            ..LoopOptions::default()
        },
    )
    .await;
    let frames = frames
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    (result, frames, requests, session)
}

/// JSON-mode parity: a run whose second request crosses the threshold emits
/// exactly one `compaction_start` / `compaction_end` pair, with the upstream
/// reason and the wire result, and the next request carries the REBUILT
/// (summary-first) context. Before the print host emitted the lifecycle,
/// `--mode json` saw NOTHING for a run that compacted.
#[tokio::test(flavor = "current_thread")]
async fn print_json_stream_carries_the_threshold_compaction_lifecycle() {
    let (result, frames, requests, session) = run_print_compaction_probe(200_000).await;
    assert_eq!(result.turns, 2, "tool-call turn, then the terminal turn");
    assert_eq!(
        frames.len(),
        2,
        "exactly one start/end pair for one threshold run: {frames:?}"
    );
    assert_eq!(
        frames[0],
        serde_json::json!({ "type": "compaction_start", "reason": "threshold" })
    );
    let end = &frames[1];
    assert_eq!(end["type"], serde_json::json!("compaction_end"));
    assert_eq!(end["reason"], serde_json::json!("threshold"));
    assert_eq!(end["aborted"], serde_json::json!(false));
    assert_eq!(end["willRetry"], serde_json::json!(false));
    assert!(
        end.get("errorMessage").is_none(),
        "a successful run carries no errorMessage: {end}"
    );
    let wire = &end["result"];
    assert!(
        wire["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("COMPACTION SUMMARY")),
        "the wire result carries the summarizer text: {wire}"
    );
    assert!(wire["tokensBefore"].is_u64(), "{wire}");
    assert!(wire["estimatedTokensAfter"].is_u64(), "{wire}");
    assert!(wire["details"]["readFiles"].is_array(), "{wire}");
    // The compaction entry was persisted for the next turn / `--continue`.
    assert_eq!(stored_compactions(&session).await, 1);
    // The second provider request carries the rebuilt context: the framed
    // summary first, not the original user message. The loop's tool
    // declaration (the entry right after the user prompt, see the sibling
    // below-threshold test) belongs to the summarized split-turn prefix, so
    // the rebuilt context is exactly summary + retained tool turn.
    let second = &requests[1];
    assert_eq!(
        second.len(),
        3,
        "summary, tool-call assistant, tool result: {second:?}"
    );
    assert!(
        message_text(&second[0]).contains("COMPACTION SUMMARY"),
        "the post-compaction request starts with the summary: {second:?}"
    );
}

/// The below-threshold half: a run that never crosses the threshold emits
/// NEITHER frame — the no-op stays silent instead of reporting a compaction
/// that did not run — and persists no compaction entry.
#[tokio::test(flavor = "current_thread")]
async fn print_json_stream_stays_silent_below_the_threshold() {
    let (result, frames, requests, session) = run_print_compaction_probe(10).await;
    assert_eq!(result.turns, 2, "tool-call turn, then the terminal turn");
    assert!(
        frames.is_empty(),
        "a sub-threshold run emits no compaction frames: {frames:?}"
    );
    assert_eq!(stored_compactions(&session).await, 0);
    // The un-compacted transcript still reaches the second request: user,
    // the loop's tool declaration, the tool-call assistant, and its result.
    //
    // The declaration is the FOURTH entry the fixture provably produces:
    // `run_with_options` fills `context.tools` from the registry when the
    // caller passes `None` (agent_loop.rs:758-777), and `declare_tool_changes`
    // then appends the `toolsAdded` system message before the first request
    // (agent_loop.rs:930-958) — the same injection every fresh production
    // print run gets. The terminal reply is NOT among these: it is produced
    // by the second request, and a request capture can never carry its own
    // answer.
    let second = &requests[1];
    assert_eq!(
        second.len(),
        4,
        "user, tool declaration, tool-call assistant, tool result: {second:?}"
    );
    assert!(message_text(&second[0]).contains("compaction probe prompt"));
    assert!(
        matches!(&second[1], Message::System(system)
        if system.tools_added.as_ref().is_some_and(|tools| {
            tools.iter().any(|tool| tool.name == "background-probe")
        })),
        "the loop's toolsAdded declaration is the second entry: {second:?}"
    );
    assert!(
        matches!(&second[2], Message::Assistant(assistant)
            if assistant.stop_reason == StopReason::ToolUse),
        "the tool-call assistant is the third entry: {second:?}"
    );
    assert!(
        matches!(&second[3], Message::ToolResult(result)
            if result.tool_name == "background-probe"),
        "the tool result is the fourth entry: {second:?}"
    );
    assert!(
        !second
            .iter()
            .any(|message| message_text(message).contains("COMPACTION SUMMARY")),
        "no summary may appear in a sub-threshold run: {second:?}"
    );
}

/// The script of the stale-snapshot regression. Request 0 answers with a
/// SMALL tool turn: its assistant records a usage UNDER the gate, so the next
/// threshold check must stay silent (the second compaction has to happen
/// AFTER the next tool turn, not here). Request 1 answers with a LARGE tool
/// turn whose text makes the character estimate cross the gate even when a
/// stale snapshot falls back to it; its assistant also records an
/// over-window usage, so the fresh-metadata path triggers on usage. The third
/// request stops.
fn scripted_double_compaction_stream(requests: Arc<Mutex<Vec<Vec<Message>>>>) -> StreamFn {
    Box::new(move |request: ProviderRequest| {
        let requests = Arc::clone(&requests);
        Box::pin(async move {
            let index = {
                let mut captured = requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                captured.push(request.context.messages.clone());
                captured.len() - 1
            };
            // A timestamp taken now keeps the usage ordered after every
            // earlier message (`last_assistant_usage_info` rejects usage
            // recorded before a later prefix message).
            let timestamp = pir::ai::util::now_ms();
            if index < 2 {
                let call_id = format!("call-compaction-{}", index + 1);
                let (text, usage_tokens) = if index == 0 {
                    ("small turn".to_owned(), 10)
                } else {
                    ("turn payload ".repeat(150), 200_000)
                };
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    content: vec![
                        ContentBlock::Text(TextContent {
                            text,
                            text_signature: None,
                        }),
                        ContentBlock::ToolCall(ToolCall {
                            id: call_id,
                            name: "background-probe".into(),
                            arguments: BTreeMap::from([("value".into(), serde_json::json!(index))]),
                            thought_signature: None,
                            namespace: None,
                            arguments_error: None,
                        }),
                    ],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input: usage_tokens,
                        total_tokens: usage_tokens,
                        ..Usage::default()
                    },
                    timestamp,
                    ..AssistantMessage::default()
                }
            } else {
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "print done".into(),
                        text_signature: None,
                    })],
                    stop_reason: StopReason::Stop,
                    timestamp,
                    ..AssistantMessage::default()
                }
            }
        })
    })
}

/// The production print shape of the stale-snapshot regression: the session
/// carries a seeded user/assistant exchange whose usage already crosses the
/// gate (so the FIRST compaction happens at turn 0, with a persisted cut
/// point), the prompt is persisted, the hook receives the prompt-start
/// projection snapshot (metadata + branch) exactly like `run_print`, and the
/// loop's own messages are persisted per `message_end` (the production sink),
/// so the session and the hook's fresh metadata agree after a compaction.
async fn run_print_double_compaction_probe() -> (
    pir::agent::agent_loop::LoopResult,
    Vec<serde_json::Value>,
    Vec<Vec<Message>>,
    SharedSession,
) {
    let frames: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let session = in_memory_print_session("compaction-stale-snapshot");
    let calls = Arc::new(Mutex::new(0usize));
    // Gate = context_window - reserve = 200 tokens: the seeded conversation's
    // assistant usage and the LARGE second tool turn cross it; the small first
    // tool turn (10 tokens) stays below it, so the second compaction can only
    // happen after request 1.
    let mut model = test_model();
    model.context_window = 1_000;
    let registry = {
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(CompactionSummaryProvider {
            model: model.clone(),
            calls: Arc::clone(&calls),
        }));
        Arc::new(registry)
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BackgroundProbe::new(
        Duration::ZERO,
        "tool output",
    )));
    let mut deps = print_loop_deps(
        Arc::new(tools),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        PathBuf::from("."),
        None,
        None,
        None,
        ThinkingLevel::Off,
    );
    deps.stream = scripted_double_compaction_stream(Arc::clone(&requests));
    // The prompt-start projection exactly as `run_print` builds it: the
    // session already carries a conversation whose last assistant usage is
    // over the gate (so the FIRST threshold check — before request 0 —
    // compacts against a PERSISTED cut point, the prompt user entry), then
    // the prompt is persisted and the metadata snapshot is taken from the
    // resulting projection.
    type ProjectedSnapshot = (
        Option<(Arc<Vec<ProjectionEntryMeta>>, Arc<Vec<PathEntryMeta>>)>,
        Vec<Message>,
    );
    let (projected, messages): ProjectedSnapshot = {
        let mut guard = session.lock().await;
        let manager = guard.as_mut().expect("in-memory session");
        let now = pir::ai::util::now_ms();
        manager
            .append_message(Message::User(UserMessage {
                role: UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: "earlier prompt".to_owned(),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                timestamp: now,
                sections: None,
            }))
            .expect("seed earlier user message");
        manager
            .append_message(Message::Assistant(AssistantMessage {
                api: "fake".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "earlier answer".to_owned(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                usage: Usage {
                    input: 200_000,
                    total_tokens: 200_000,
                    ..Usage::default()
                },
                timestamp: now,
                ..AssistantMessage::default()
            }))
            .expect("seed earlier assistant message");
        manager
            .append_message(Message::User(UserMessage {
                role: UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: "compaction probe prompt".to_owned(),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                timestamp: now,
                sections: None,
            }))
            .expect("persist prompt");
        let projection = manager.build_session_projection().expect("projection");
        let branch = manager.branch_path_meta(None);
        let metas = ProjectionEntryMeta::from_projection(&projection.entries);
        (
            Some((Arc::new(metas), Arc::new(branch))),
            projection.messages,
        )
    };
    let hook = print_compaction_hook(
        CompactionConfig {
            enabled: true,
            reserve_tokens: 800,
            keep_recent_tokens: 1,
        },
        model,
        registry,
        None,
        ThinkingLevel::Off,
        pir::ai::types::ProviderStreamHooks::default(),
        Arc::clone(&session),
        projected,
        None,
        Some(recording_session_event_sink(Arc::clone(&frames))),
    );
    // The production per-message persistence sink (`run_print`): every
    // loop-produced message lands in the session at its `message_end`, so the
    // post-compaction metadata rebuild reads the entries the rebuilt context
    // actually came from.
    let persist_message: PersistMessageFn = Arc::new({
        let session = Arc::clone(&session);
        move |message: Message| {
            let session = Arc::clone(&session);
            Box::pin(async move {
                let mut guard = session.lock().await;
                if let Some(manager) = guard.as_mut() {
                    manager
                        .append_message(message)
                        .expect("persist loop message");
                }
            })
        }
    });
    let result = run_with_options(
        deps,
        Context {
            system_prompt: None,
            messages,
            tools: None,
        },
        None,
        LoopOptions {
            compaction: Some(hook),
            persist_message: Some(persist_message),
            ..LoopOptions::default()
        },
    )
    .await;
    let frames = frames
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    (result, frames, requests, session)
}

/// Regression: a SECOND threshold compaction inside one print run must cut
/// over the LIVE session metadata, not the prompt-start snapshot. Before the
/// fix `print_compaction_hook` kept slicing its pre-compaction entry
/// counts/ids against the rebuilt context, so the second cut point landed on a
/// pre-boundary id (duplicating summarized history) or on a synthetic tail
/// entry with no id — which `append_compaction` persists as SELF-RETAINING
/// (`firstKeptEntryId == the compaction entry's own id`), dropping the newest
/// kept messages from every later turn / `--continue` projection.
#[tokio::test(flavor = "current_thread")]
async fn print_second_threshold_compaction_uses_the_live_session_metadata() {
    let (result, frames, requests, session) = run_print_double_compaction_probe().await;
    assert_eq!(result.turns, 3, "two tool turns, then the terminal turn");
    assert_eq!(
        frames.len(),
        4,
        "one start/end pair per threshold compaction: {frames:?}"
    );
    for pair in frames.chunks(2) {
        assert_eq!(
            pair[0],
            serde_json::json!({ "type": "compaction_start", "reason": "threshold" })
        );
        assert_eq!(pair[1]["type"], serde_json::json!("compaction_end"));
        assert_eq!(pair[1]["reason"], serde_json::json!("threshold"));
        assert!(
            pair[1].get("errorMessage").is_none(),
            "a successful compaction carries no errorMessage: {}",
            pair[1]
        );
    }
    assert_eq!(stored_compactions(&session).await, 2);
    // The third request carries the SECOND summary followed by the second
    // tool turn (the loop's rebuilt context), proving both compactions ran.
    let third = &requests[2];
    assert!(
        message_text(&third[0]).contains("COMPACTION SUMMARY"),
        "the third request starts with the second summary: {third:?}"
    );
    // The persisted second tool-call assistant entry is the cut point the
    // second compaction must keep.
    let (second_call_entry_id, compaction_entries) = {
        let guard = session.lock().await;
        let manager = guard.as_ref().expect("session");
        let entries = manager.entries();
        let second_call_entry_id = entries
            .iter()
            .find_map(|entry| match entry {
                FileEntry::Message(message) => match &message.message {
                    StoredMessage::Assistant(assistant)
                        if assistant.content.iter().any(|block| {
                            matches!(block, ContentBlock::ToolCall(call)
                                if call.id == "call-compaction-2")
                        }) =>
                    {
                        Some(message.id.clone())
                    }
                    _ => None,
                },
                _ => None,
            })
            .expect("the second tool call must be persisted by the message sink");
        let compaction_entries: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                FileEntry::Compaction(compaction) => Some(compaction.clone()),
                _ => None,
            })
            .collect();
        (second_call_entry_id, compaction_entries)
    };
    assert_eq!(compaction_entries.len(), 2);
    let second_compaction = &compaction_entries[1];
    assert_ne!(
        second_compaction.first_kept_entry_id, second_compaction.id,
        "a `None` cut point self-retains and drops the newest kept messages"
    );
    assert_eq!(
        second_compaction.first_kept_entry_id, second_call_entry_id,
        "the second compaction must keep the LIVE cut point entry (the second \
         tool call), not a pre-compaction entry id"
    );
    assert_eq!(
        frames[3]["result"]["firstKeptEntryId"],
        serde_json::json!(second_call_entry_id),
        "the wire compaction_end carries the live kept-range entry id"
    );
    // The next prompt / `--continue` projection keeps the second tool turn and
    // does not resurrect the already-summarized prompt.
    let projection_messages = {
        let guard = session.lock().await;
        guard
            .as_ref()
            .expect("session")
            .build_session_projection()
            .expect("projection")
            .messages
    };
    assert!(
        projection_messages.iter().any(|message| {
            matches!(message, Message::Assistant(assistant)
            if assistant.content.iter().any(|block| {
                matches!(block, ContentBlock::ToolCall(call)
                    if call.id == "call-compaction-2")
            }))
        }),
        "the second tool call must survive in the persisted projection: {projection_messages:?}"
    );
    assert!(
        projection_messages.iter().any(|message| {
            matches!(message, Message::ToolResult(result)
                if result.tool_call_id == "call-compaction-2")
        }),
        "the second tool result must survive in the persisted projection: {projection_messages:?}"
    );
    assert!(
        !projection_messages
            .iter()
            .any(|message| message_text(message).contains("compaction probe prompt")),
        "the summarized prompt must not reappear inside the kept range: {projection_messages:?}"
    );
}

/// The append-failure probe's script: requests 0 and 1 answer with a tool call
/// whose usage crosses the gate, request 2 stops. The FIRST threshold
/// compaction's session append fails (injected through the session manager's
/// test seam), the next turn must retry.
fn scripted_compaction_append_retry_stream(requests: Arc<Mutex<Vec<Vec<Message>>>>) -> StreamFn {
    Box::new(move |request: ProviderRequest| {
        let requests = Arc::clone(&requests);
        Box::pin(async move {
            let index = {
                let mut captured = requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                captured.push(request.context.messages.clone());
                captured.len() - 1
            };
            // A timestamp taken now keeps each turn's usage ordered after the
            // earlier messages (`last_assistant_usage_info`).
            let timestamp = pir::ai::util::now_ms();
            if index < 2 {
                let call_id = format!("call-append-failure-{}", index + 1);
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: call_id,
                        name: "background-probe".into(),
                        arguments: BTreeMap::from([("value".into(), serde_json::json!(index))]),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    })],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input: 200_000,
                        total_tokens: 200_000,
                        ..Usage::default()
                    },
                    timestamp,
                    ..AssistantMessage::default()
                }
            } else {
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "print done".into(),
                        text_signature: None,
                    })],
                    stop_reason: StopReason::Stop,
                    timestamp,
                    ..AssistantMessage::default()
                }
            }
        })
    })
}

/// Run the retry script through the PRODUCTION print compaction hook against a
/// FILE-BACKED session (the `fail_next_append_for_test` seam only fires when
/// `persist_entry` actually writes a file), after seeding one persisted user
/// message that flushes the session file. Returns the loop result, the recorded
/// frames, the request contexts, and the session.
async fn run_print_append_failure_probe(
    session_dir: &Path,
) -> (
    pir::agent::agent_loop::LoopResult,
    Vec<serde_json::Value>,
    Vec<Vec<Message>>,
    SharedSession,
) {
    let frames: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let session: SharedSession = Arc::new(tokio::sync::Mutex::new(Some(
        SessionManager::create(&session_dir.to_string_lossy(), session_dir)
            .expect("create session"),
    )));
    let prompt = Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: "compaction probe prompt".to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    });
    {
        let mut guard = session.lock().await;
        let manager = guard.as_mut().expect("file-backed session");
        // The first conversation append creates and flushes the session file, so
        // the injected failure below lands on the NEXT append: the compaction
        // entry (the run installs no per-message persistence sink).
        manager.append_message(prompt.clone()).expect("seed prompt");
        // The seeded entry only exists to flush the file; take it off the leaf
        // path so the threshold check and the cut basis run over the live
        // context (`print_compaction_threshold_met`'s plain-estimate path, which
        // trusts the assistant usage), exactly like the prompt-only probes.
        manager.reset_leaf();
        manager.fail_next_append_for_test(AppendFailure::BeforeWrite);
    }
    let calls = Arc::new(Mutex::new(0usize));
    let registry = {
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(CompactionSummaryProvider {
            model: test_model(),
            calls: Arc::clone(&calls),
        }));
        Arc::new(registry)
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BackgroundProbe::new(
        Duration::ZERO,
        "tool output",
    )));
    let mut deps = print_loop_deps(
        Arc::new(tools),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        PathBuf::from("."),
        None,
        None,
        None,
        ThinkingLevel::Off,
    );
    deps.stream = scripted_compaction_append_retry_stream(Arc::clone(&requests));
    let hook = print_compaction_hook(
        CompactionConfig {
            enabled: true,
            reserve_tokens: 1_000,
            keep_recent_tokens: 1,
        },
        test_model(),
        registry,
        None,
        ThinkingLevel::Off,
        pir::ai::types::ProviderStreamHooks::default(),
        Arc::clone(&session),
        None,
        None,
        Some(recording_session_event_sink(Arc::clone(&frames))),
    );
    let result = run_with_options(
        deps,
        Context {
            system_prompt: None,
            messages: vec![prompt],
            tools: None,
        },
        None,
        LoopOptions {
            compaction: Some(hook),
            ..LoopOptions::default()
        },
    )
    .await;
    let frames = frames
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    (result, frames, requests, session)
}

/// Regression (print append-failure parity with `agent_session/compact.rs`): a
/// threshold compaction whose session append fails must NOT return the rebuilt
/// (shortened) context, must NOT report a successful `compaction_end`, and must
/// NOT persist a compaction entry. The failed event pair is
/// `compaction_end` (error form, `willRetry: false`) and the run continues
/// un-compacted; the next turn re-checks the threshold and retries, and that
/// retry succeeds.
///
/// Before the fix the failure was only printed to stderr: the hook returned the
/// summary-first context and emitted a successful `compaction_end` while no
/// compaction boundary existed in session storage, so a later turn /
/// `--continue` saw a context that could never be reconstructed from the
/// session file.
#[tokio::test(flavor = "current_thread")]
async fn print_append_failure_keeps_context_and_retries_next_turn() {
    let temp = TempDir::new("compaction-append-failure");
    let (result, frames, requests, session) = run_print_append_failure_probe(&temp.0).await;
    assert_eq!(result.turns, 3, "two tool turns, then the terminal turn");
    assert_eq!(
        frames.len(),
        4,
        "one start/end pair per threshold run (the failed one and the retry): {frames:?}"
    );
    assert_eq!(
        frames[0],
        serde_json::json!({ "type": "compaction_start", "reason": "threshold" })
    );
    let failed = &frames[1];
    assert_eq!(failed["type"], serde_json::json!("compaction_end"));
    assert_eq!(failed["reason"], serde_json::json!("threshold"));
    assert_eq!(failed["aborted"], serde_json::json!(false));
    assert_eq!(failed["willRetry"], serde_json::json!(false));
    assert!(
        failed["result"].is_null(),
        "an append failure must not report a successful compaction result: {failed}"
    );
    let error_message = failed["errorMessage"]
        .as_str()
        .unwrap_or_else(|| panic!("the failed compaction carries an errorMessage: {failed}"));
    assert!(
        error_message.starts_with("Auto-compaction failed: "),
        "the failure uses the upstream message: {error_message}"
    );
    assert!(
        error_message.contains("injected session append failure"),
        "the reported failure is the append error, not a summarizer error: {error_message}"
    );
    // The retry (the NEXT turn's threshold check) runs its own pair and
    // succeeds.
    assert_eq!(
        frames[2],
        serde_json::json!({ "type": "compaction_start", "reason": "threshold" })
    );
    let succeeded = &frames[3];
    assert_eq!(succeeded["type"], serde_json::json!("compaction_end"));
    assert!(
        succeeded["result"].is_object(),
        "the retried compaction reports its wire result: {succeeded}"
    );
    assert!(
        succeeded.get("errorMessage").is_none(),
        "the retry succeeded, so it carries no errorMessage: {succeeded}"
    );
    // The failed append persisted nothing: only the retry's entry exists.
    assert_eq!(
        stored_compactions(&session).await,
        1,
        "the failed append must not leave a compaction entry behind"
    );
    // The turn after the failed append was sent the ORIGINAL context (no
    // summary), proving the hook returned the pre-compaction context unchanged.
    let second = &requests[1];
    assert!(
        second
            .iter()
            .any(|message| message_text(message).contains("compaction probe prompt")),
        "the post-failure request still carries the original prompt: {second:?}"
    );
    assert!(
        !second
            .iter()
            .any(|message| message_text(message).contains("COMPACTION SUMMARY")),
        "the failed append must not shorten the context: {second:?}"
    );
    // The successful retry rebuilds the context for the following request.
    let third = &requests[2];
    assert!(
        message_text(&third[0]).contains("COMPACTION SUMMARY"),
        "the retried compaction's rebuilt context starts with the summary: {third:?}"
    );
    drop(session);
}

/// The paged fast path's pre-run trigger is the SAME decision `compact_paged`
/// takes: `threshold_met` is false below the gate (the `ThresholdNotMet`
/// no-op the print hook must keep silent) and true above it — checked on a
/// real paged session, so the window projection (never the skipped prefix) is
/// what the decision reads.
#[test]
fn paged_threshold_met_is_the_compaction_trigger() {
    let dir = TempDir::new("paged-threshold");
    let mut manager = SessionManager::create("C:\\repo", &dir.0).expect("create session");
    // Payload entries the small page caps cannot hold: the open below parses
    // only a tail window (`is_paged()`).
    for turn in 0..8 {
        manager
            .append_message(Message::User(UserMessage {
                role: UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: format!("turn {turn}\n{}", "paged payload line\n".repeat(256)),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                timestamp: 1,
                sections: None,
            }))
            .expect("append fixture turn");
    }
    let path = manager.path().to_path_buf();
    drop(manager);
    let paged = SessionManager::open_paged(&path, 4 * 1024, 2).expect("open fixture paged");
    assert!(paged.is_paged(), "the caps must leave a tail window");

    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 1_000,
        keep_recent_tokens: 1,
    };
    // A window far above the estimate: the trigger does not fire — exactly
    // the `ThresholdNotMet` no-op that must not be framed.
    let mut huge = test_model();
    huge.context_window = 10_000_000;
    assert!(
        !pir::compaction::paged::threshold_met(&paged, &huge, &config).expect("projection"),
        "the resident-window estimate is far below a 10M-token window"
    );
    // Window 0: the gate sits at the saturating floor (0), and the resident
    // window carries a positive estimate — the trigger fires.
    let mut zero = test_model();
    zero.context_window = 0;
    assert!(
        pir::compaction::paged::threshold_met(&paged, &zero, &config).expect("projection"),
        "a 0-token window makes any positive estimate cross the gate"
    );
}

/// Print mode is the last host that used to run unbounded turns. A measured
/// degenerate loop (one logq leg issued the same bash call 489 times, 511
/// turns / 17.4M tokens) makes that a real cost. Non-interactive hosts now
/// default to `DEFAULT_AGENT_MAX_TURNS` (1000, `PIR_MAX_TURNS`-aware); the
/// interactive TUI keeps the 100-turn guard.
#[test]
fn print_mode_caps_runaway_turns_like_the_other_hosts() {
    let deps = print_loop_deps(
        Arc::new(ToolRegistry::new()),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        PathBuf::from("."),
        None,
        None,
        None,
        ThinkingLevel::Off,
    );
    assert_eq!(
        deps.max_turns,
        pir::config::defaults::DEFAULT_AGENT_MAX_TURNS,
        "a runaway print-mode model loop must stay bounded by the non-interactive cap"
    );
}

// ---------------------------------------------------------------------------
// Print prompt cache: the `mcp_servers` section (CL18 audit item 11)
// ---------------------------------------------------------------------------

/// The print host renders the `mcp_servers` section like the session host
/// (upstream's `before_agent_start` re-render, `extensions/mcp/index.ts:1176`):
/// [`pir::coding_agent::print::PrintPromptCache`] inserts it as a trailing
/// `<mcp_servers>` section, with no MCP it stays byte-identical to the build
/// without one, and a refreshed section re-renders on the NEXT `sync` only.
#[test]
fn print_prompt_cache_renders_and_refreshes_the_mcp_section() {
    let dir = TempDir::new("print-mcp-section");
    let registry = ToolRegistry::new();

    let mcp =
        "MCP servers configured for this session; their tools are named mcp__<server>__<tool>.\n\
               - mcp__docs (codemode): connected · 2 tools\n\
               - mcp__slow (deferred): connecting";
    let mut cache = pir::coding_agent::print::PrintPromptCache::new_with_mcp_servers(
        None,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        dir.0.clone(),
        None,
        Some(mcp.to_owned()),
    );
    let prompt = cache.sync(&registry, &[], None);
    assert!(
        prompt.contains("<mcp_servers>") && prompt.contains("</mcp_servers>"),
        "the print prompt carries the section: {prompt}"
    );
    assert!(
        prompt.contains("- mcp__docs (codemode): connected · 2 tools"),
        "the ready server is listed: {prompt}"
    );
    // The section is appended AFTER the built-ins (custom sections are last).
    let mcp_at = prompt.find("<mcp_servers>").expect("section present");
    for builtin in ["<tools>", "<rules>", "<cwd>"] {
        let at = prompt.find(builtin).expect("builtin section present");
        assert!(at < mcp_at, "{builtin} precedes the mcp_servers section");
    }

    // Steady state, unchanged section: byte-identical bytes.
    assert_eq!(
        prompt,
        cache.sync(&registry, &[], None),
        "an unchanged MCP section reuses the cached prompt"
    );

    // A transition (pending -> ready) lands on the NEXT sync.
    cache.set_mcp_servers(Some(
        "MCP servers configured for this session; their tools are named mcp__<server>__<tool>.\n\
         - mcp__docs (codemode): connected · 2 tools\n\
         - mcp__slow (deferred): connected · 1 tool"
            .to_owned(),
    ));
    let refreshed = cache.sync(&registry, &[], None);
    assert!(
        refreshed.contains("- mcp__slow (deferred): connected · 1 tool"),
        "the refreshed state lands on the next prompt: {refreshed}"
    );
    assert!(
        !refreshed.contains("mcp__slow (deferred): connecting"),
        "the stale state is gone: {refreshed}"
    );

    // Clearing it removes the section and restores the MCP-less bytes.
    cache.set_mcp_servers(None);
    let without = cache.sync(&registry, &[], None);
    assert!(!without.contains("mcp_servers"), "{without}");
    let mut plain = pir::coding_agent::print::PrintPromptCache::new(
        None,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        dir.0.clone(),
        None,
    );
    assert_eq!(
        without,
        plain.sync(&registry, &[], None),
        "an MCP-less print prompt is byte-identical to the pre-section build"
    );
}

// ---------------------------------------------------------------------------
// settings.retry in the print host (Q19 caveat: print ignored the settings)
// ---------------------------------------------------------------------------
//
// `pir print` drives `agent_loop` itself instead of an `AgentSession`, so the
// session's per-prompt `retry: session_settings().agent_retry_policy()` wiring
// (agent_session/prompt_loop.rs:882-885) never ran for it: every print run got
// the `LoopDeps` default policy, so `retry.enabled: false` and a custom
// `maxRetries` were silently ignored. These tests pin the production path —
// [`print_loop_deps`] + [`print_apply_retry`] over a parsed settings snapshot —
// by REQUEST COUNT, with the real classified terminal error the adapters emit.

/// A retryable MID-STREAM terminal (the transport rendered no verdict), built
/// through the same finalizer the real adapters use so the loop sees the
/// structural `RetryableTurn` class.
fn retryable_mid_stream_error() -> AssistantMessage {
    pir::ai::api::terminal_error_message(
        &test_model(),
        &pir::ai::error::AiError::StreamNotTerminated,
        None,
    )
}

/// The recovered turn the retried request answers with.
fn recovered_assistant() -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: "recovered".into(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

/// `print_loop_deps` + the settings-derived policy: the exact production
/// assembly `run_print` performs for every prompt run.
fn print_retry_deps(
    settings: &pir::config::settings::Settings,
    calls: Arc<AtomicUsize>,
) -> LoopDeps {
    let mut deps = print_loop_deps(
        Arc::new(ToolRegistry::new()),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        PathBuf::from("."),
        None,
        None,
        None,
        ThinkingLevel::Off,
    );
    print_apply_retry(&mut deps, settings);
    deps.stream = Box::new(move |_request: ProviderRequest| {
        let attempt = calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if attempt == 0 {
                retryable_mid_stream_error()
            } else {
                recovered_assistant()
            }
        })
    });
    deps
}

fn print_settings(value: serde_json::Value) -> pir::config::settings::Settings {
    serde_json::from_value(value).expect("parse settings fixture")
}

/// `retry.enabled: false` yields NO policy, and a retryable mid-stream
/// failure therefore ends the print run after exactly ONE request with the
/// provider's terminal error — the behavior `pir print` was missing.
#[tokio::test]
async fn print_retry_disabled_by_settings_issues_one_request() {
    let settings = print_settings(serde_json::json!({ "retry": { "enabled": false } }));
    assert!(
        print_retry_policy(&settings).is_none(),
        "a disabled retry block must resolve to no agent-level policy"
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let deps = print_retry_deps(&settings, Arc::clone(&calls));
    assert!(deps.retry.is_none(), "the policy must reach the loop deps");

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "retry.enabled:false means a single attempt"
    );
    assert_eq!(result.terminal.stop_reason, StopReason::Error);
    assert_eq!(
        result.terminal.error_message.as_deref(),
        Some("stream ended without a terminal event"),
        "the provider's terminal error is what the host reports"
    );
}

/// `retry.maxRetries: 1` is honored: the retryable failure is re-issued once
/// and the second (successful) attempt ends the same turn.
#[tokio::test]
async fn print_retry_max_retries_is_honored() {
    let settings = print_settings(serde_json::json!({
        "retry": { "maxRetries": 1, "baseDelayMs": 0, "maxAgentDelayMs": 0 }
    }));
    let policy = print_retry_policy(&settings).expect("an enabled, tuned policy");
    assert_eq!(policy.max_retries, 1, "the settings maxRetries wins");
    assert_eq!(policy.base_delay_ms, 0);

    let calls = Arc::new(AtomicUsize::new(0));
    let deps = print_retry_deps(&settings, Arc::clone(&calls));

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the initial attempt plus exactly one re-issue"
    );
    assert_eq!(result.turns, 1, "a retry is the same turn");
    assert_eq!(result.terminal.content, recovered_assistant().content);
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);
}

/// A `maxRetries` the settings cap to zero (`"maxRetries": 0`) is the same
/// single-attempt contract as `enabled: false` (upstream gates both).
#[tokio::test]
async fn print_retry_zero_max_retries_issues_one_request() {
    let settings = print_settings(serde_json::json!({ "retry": { "maxRetries": 0 } }));
    assert!(print_retry_policy(&settings).is_none());

    let calls = Arc::new(AtomicUsize::new(0));
    let deps = print_retry_deps(&settings, Arc::clone(&calls));
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.terminal.stop_reason, StopReason::Error);
}

/// The default is unchanged: an ABSENT `retry` block resolves the documented
/// upstream default (enabled, 3 retries, 2 s base, 60 s cap) instead of
/// silently disabling retries — a bare `Settings::default()` must not be read
/// as "off".
#[test]
fn print_retry_defaults_to_the_enabled_policy() {
    use pir::config::defaults::settings_defaults;

    let expected = pir::ai::util::AgentRetryPolicy {
        max_retries: settings_defaults::RETRY_MAX_RETRIES,
        base_delay_ms: settings_defaults::RETRY_BASE_DELAY_MS,
        max_agent_delay_ms: settings_defaults::RETRY_MAX_AGENT_DELAY_MS,
    };
    assert_eq!(
        print_retry_policy(&pir::config::settings::Settings::default()),
        Some(expected)
    );
    assert_eq!(expected.max_retries, 3);

    // The print builder itself keeps that policy until `run_print` applies
    // the run's settings snapshot: a bare embedder stays on the documented
    // enabled default.
    let deps = print_loop_deps(
        Arc::new(ToolRegistry::new()),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        PathBuf::from("."),
        None,
        None,
        None,
        ThinkingLevel::Off,
    );
    assert_eq!(
        deps.retry,
        Some(expected),
        "print_loop_deps must not silently disable agent-level retries"
    );

    // An explicit partial block keeps the unspecified fields on the defaults.
    let tuned = print_settings(serde_json::json!({ "retry": { "maxRetries": 2 } }));
    assert_eq!(
        print_retry_policy(&tuned),
        Some(pir::ai::util::AgentRetryPolicy {
            max_retries: 2,
            ..expected
        })
    );
}
