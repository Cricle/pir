//! Startup thinking level: clamped once at resolution/construction, and the
//! footer seeded from the SAME clamped value (PIR-BIN-074 + PIR-TUI-032).
//!
//! Upstream clamps at session creation (`createAgentSession`:
//! `thinkingLevel = clampThinkingLevel(model, thinkingLevel)`, sdk.ts:264-275),
//! and the footer paints the level the session will send. pir used to:
//!
//! - send a level the model's `thinkingLevelMap` never declares
//!   (`--thinking max` on a model without a `max` entry reached the provider
//!   as `reasoning_effort: "max"`), because `config.thinking` was installed
//!   verbatim and the only clamp lived inside `set_thinking_level` — i.e.
//!   after the first request had already gone out; and
//! - paint `thinking off` for `--thinking high` / `--model m:high`, because the
//!   footer was seeded from `settings.defaultThinkingLevel` alone while the
//!   worker received the resolved CLI/suffix level.
//!
//! The pins below drive the production helpers and the real request path
//! (`print_loop_deps` + registry stream; `AgentSession` + its registry stream)
//! and assert the level each FIRST request carries. Fixtures are fake
//! providers; no `~/.pi` configuration is consulted.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use pir::agent::agent_loop::{run_with_options, LoopOptions};
use pir::agent::tool_execution::ToolRegistry;
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::agent_session::{
    clamp_starting_thinking_level_for_model, resolve_starting_thinking_level,
    resolve_starting_thinking_level_for_model, AgentSession, AgentSessionConfig,
};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, Context, Model, ModelCost, ModelCostRates, StopReason, StreamOptions,
    TerminalReason, TextContent, ThinkingLevel,
};
use pir::coding_agent::print::print_loop_deps;
use pir::tui::app::{Footer, FooterState};
use pir::tui::terminal::startup_thinking_level;
use pir::tui::Component;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A reasoning model in the streamux A/B shape: `openai-completions`,
/// `reasoning: true`, and NO `thinkingLevelMap` — so `off|minimal|low|medium|
/// high` are supported while `xhigh`/`max` require an explicit map entry.
fn bare_reasoning_model(provider: &str) -> Model {
    Model {
        id: "mock-model".into(),
        name: "mock-model".into(),
        api: "openai-completions".into(),
        provider: provider.into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        reasoning: true,
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
        context_window: 100_000,
        max_tokens: 8_192,
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

/// A model whose map nulls `medium` (unsupported) and names `low`/`high`.
fn medium_nulled_model(provider: &str) -> Model {
    let mut model = bare_reasoning_model(provider);
    model.thinking_level_map = Some(BTreeMap::from([
        ("medium".to_owned(), None),
        ("low".to_owned(), Some("low".to_owned())),
        ("high".to_owned(), Some("high".to_owned())),
    ]));
    model
}

fn text_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "openai-completions".into(),
        provider: "mock".into(),
        model: "mock-model".into(),
        content: vec![pir::ai::types::ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

/// Records the `StreamOptions.reasoning` of every request and completes with
/// one plain stop (one-turn runs).
struct LevelRecorder {
    model: Model,
    seen: Arc<Mutex<Vec<Option<ThinkingLevel>>>>,
}

impl Provider for LevelRecorder {
    #[allow(clippy::misnamed_getters)]
    fn id(&self) -> &str {
        &self.model.provider
    }

    fn name(&self) -> &str {
        "level recorder"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        // A keyless fake: `stream_resolving_auth*` dispatches anyway.
        Vec::new()
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
            text_message("ok"),
        ));
        stream
    }
}

fn registry_with(
    model: &Model,
    seen: Arc<Mutex<Vec<Option<ThinkingLevel>>>>,
) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(LevelRecorder {
        model: model.clone(),
        seen,
    }));
    Arc::new(registry)
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pir-startup-thinking-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}

// ---------------------------------------------------------------------------
// (a) Print path: an undeclared level is clamped BEFORE the first request
// ---------------------------------------------------------------------------

/// The print host resolves its starting level through
/// [`resolve_starting_thinking_level_for_model`] and bakes it into
/// `LoopDeps::thinking_level` + `StreamOptions.reasoning` (print.rs). Drive
/// that exact assembly into the real registry stream: a level the model's map
/// does not declare (`max`, `xhigh`) must reach the provider as the nearest
/// supported one, and the recorded assistant message must carry it too.
#[tokio::test(flavor = "current_thread")]
async fn print_first_request_clamps_a_level_the_map_does_not_declare() {
    let model = bare_reasoning_model("mock");
    let seen: Arc<Mutex<Vec<Option<ThinkingLevel>>>> = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with(&model, Arc::clone(&seen));

    for (requested, expected) in [
        (ThinkingLevel::Max, ThinkingLevel::High),
        (ThinkingLevel::XHigh, ThinkingLevel::High),
    ] {
        // The production resolution: `--thinking <requested>`, no per-model
        // override, no settings default.
        let level = resolve_starting_thinking_level_for_model(&model, Some(requested), None, None);
        assert_eq!(
            level, expected,
            "the print resolution must clamp {requested:?} to the model's ladder"
        );

        let deps = print_loop_deps(
            Arc::new(ToolRegistry::new()),
            ToolScheduler::new(ToolSchedulerConfig::default()),
            std::path::PathBuf::from("."),
            None,
            None,
            None,
            level,
        )
        .with_provider_unchecked(
            Arc::clone(&registry),
            model.clone(),
            StreamOptions {
                reasoning: Some(level),
                ..StreamOptions::default()
            },
        );
        let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
        assert_eq!(
            result.terminal.thinking_level,
            Some(expected),
            "the recorded assistant thinkingLevel must match the clamped request"
        );
    }

    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        seen,
        vec![Some(ThinkingLevel::High), Some(ThinkingLevel::High)],
        "the FIRST request must carry the clamped effort, never max/xhigh verbatim"
    );
}

/// A `null`-mapped level is unsupported too: the shared settings default
/// (`medium`) on a model whose map nulls `medium` clamps to the next
/// supported level up (`high`), exactly like upstream's session creation.
#[tokio::test(flavor = "current_thread")]
async fn print_first_request_clamps_a_null_mapped_settings_default() {
    let model = medium_nulled_model("mock");
    assert_eq!(
        resolve_starting_thinking_level_for_model(&model, None, None, Some(ThinkingLevel::Medium)),
        ThinkingLevel::High,
        "a null-mapped level must clamp before it is sent"
    );
    // The shared (unclamped) resolver still answers the *requested* level;
    // the model-aware form is what the hosts must call.
    assert_eq!(
        resolve_starting_thinking_level(None, None, Some(ThinkingLevel::Medium)),
        ThinkingLevel::Medium
    );
}

// ---------------------------------------------------------------------------
// (b) TUI / session-build path: constructed session clamps before request 1
// ---------------------------------------------------------------------------

/// [`AgentSession`] is built with the level the TUI worker passes
/// (`WorkerOverrides.thinking` -> `config.thinking`). A level the model does
/// not declare must be installed clamped at CONSTRUCTION: it is readable
/// before the first prompt, and the first prompt's provider request already
/// carries the supported effort (the only clamp used to live inside
/// `set_thinking_level`, i.e. after the request had gone out).
#[tokio::test(flavor = "current_thread")]
async fn session_build_clamps_before_the_first_request() {
    let dir = temp_dir("session-build");
    let model = bare_reasoning_model("mock");
    let seen: Arc<Mutex<Vec<Option<ThinkingLevel>>>> = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with(&model, Arc::clone(&seen));

    let session = AgentSession::new_with_agent_dir(
        AgentSessionConfig {
            performance: Default::default(),
            provider_registry: Arc::clone(&registry),
            model: model.clone(),
            api_key: None,
            thinking: ThinkingLevel::Max,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 2,
            system_prompt: Some("startup clamp".into()),
            cwd: Some(dir.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        &dir,
    );
    // The effective level is accurate BEFORE any run started.
    assert_eq!(
        session.thinking_level(),
        ThinkingLevel::High,
        "construction must install the clamped level (max is not mapped)"
    );

    let terminal = session
        .prompt("hello", Arc::new(|_event| {}))
        .await
        .expect("prompt");
    assert_eq!(
        terminal.thinking_level,
        Some(ThinkingLevel::High),
        "the recorded assistant thinkingLevel must be the clamped one"
    );
    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        seen,
        vec![Some(ThinkingLevel::High)],
        "the FIRST request must carry the clamped effort"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same construction on a model that DOES map `max` keeps it (the clamp
/// must not flatten a supported level), and a non-reasoning model collapses
/// to `off` like upstream.
#[tokio::test(flavor = "current_thread")]
async fn session_build_preserves_supported_levels_and_off_for_non_reasoning() {
    let mut mapped = bare_reasoning_model("mock");
    mapped.thinking_level_map = Some(BTreeMap::from([("max".to_owned(), Some("max".to_owned()))]));
    assert_eq!(
        clamp_starting_thinking_level_for_model(&mapped, ThinkingLevel::Max),
        ThinkingLevel::Max,
        "a mapped max stays max"
    );

    let mut plain = bare_reasoning_model("mock");
    plain.reasoning = false;
    assert_eq!(
        clamp_starting_thinking_level_for_model(&plain, ThinkingLevel::High),
        ThinkingLevel::Off,
        "a non-reasoning model supports only off"
    );
}

// ---------------------------------------------------------------------------
// (c) The footer's `thinking_level` equals the clamped resolved level
// ---------------------------------------------------------------------------

/// The footer and the session build BOTH derive their level from
/// [`startup_thinking_level`], for every source of the resolution chain:
/// `--thinking`, a `--model m:level` suffix (also delivered as `resolved`),
/// a settings default, and the no-setting case. On a model that does not map
/// `max`, `--thinking max` must paint `high` — what the wire carries.
#[test]
fn footer_level_equals_the_clamped_resolved_level() {
    let model = bare_reasoning_model("mock");

    // (source label, resolved from CLI/suffix, settings default, expected)
    let cases = [
        (
            "--thinking high",
            Some(ThinkingLevel::High),
            None,
            ThinkingLevel::High,
        ),
        (
            "--model m:high",
            Some(ThinkingLevel::High),
            None,
            ThinkingLevel::High,
        ),
        (
            "--thinking max (unmapped)",
            Some(ThinkingLevel::Max),
            None,
            ThinkingLevel::High,
        ),
        (
            "settings default high",
            None,
            Some(ThinkingLevel::High),
            ThinkingLevel::High,
        ),
        (
            "settings default off",
            None,
            Some(ThinkingLevel::Off),
            ThinkingLevel::Medium,
        ),
        ("no setting", None, None, ThinkingLevel::Medium),
        // The CLI wins over the settings default, and is still clamped.
        (
            "--thinking high over settings max",
            Some(ThinkingLevel::High),
            Some(ThinkingLevel::Max),
            ThinkingLevel::High,
        ),
    ];

    for (label, resolved, settings, expected) in cases {
        let level = startup_thinking_level(&model, resolved, settings);
        assert_eq!(level, expected, "{label}: resolved level");

        // The footer's field is exactly this level's wire key.
        let footer = FooterState {
            cwd: "/work".into(),
            model_id: model.id.clone(),
            model_provider: model.provider.clone(),
            model_reasoning: model.reasoning,
            thinking_level: level.wire_key().to_string(),
            ..FooterState::default()
        };
        assert_eq!(
            footer.thinking_level,
            expected.wire_key(),
            "{label}: footer must paint the level the first request sends"
        );

        // The session build clamps the SAME value (idempotent at build), so
        // the painted level and `config.thinking` cannot drift.
        assert_eq!(
            clamp_starting_thinking_level_for_model(&model, level),
            level,
            "{label}: the clamp is idempotent (footer == session config)"
        );
        assert_eq!(
            startup_thinking_level(&model, Some(level), settings),
            level,
            "{label}: re-resolving an already resolved level keeps it"
        );
    }
}

/// The painted row itself: a footer seeded this way renders the level, so a
/// `--thinking high` boot never reads `thinking off` (PIR-TUI-032).
#[test]
fn footer_renders_the_startup_level_from_the_first_frame() {
    let _guard = footer_test_lock();
    pir::tui::theme::set_theme("dark");

    let model = bare_reasoning_model("mock");
    let cases = [
        (
            // exactly what `run()` resolves for `--thinking high`
            Some(ThinkingLevel::High),
            None,
            "high",
        ),
        (
            // `--model mock/mock-model:high`
            Some(ThinkingLevel::High),
            None,
            "high",
        ),
        (
            // `--thinking max` on a model that does not map max
            Some(ThinkingLevel::Max),
            None,
            "high",
        ),
        (None, Some(ThinkingLevel::High), "high"),
        (None, None, "medium"),
    ];

    for (resolved, settings, expected) in cases {
        let level = startup_thinking_level(&model, resolved, settings);
        let footer = Footer::new(FooterState {
            cwd: "/work/project".into(),
            model_id: model.id.clone(),
            model_provider: model.provider.clone(),
            model_reasoning: true,
            thinking_level: level.wire_key().to_string(),
            ..FooterState::default()
        });
        let rows = footer
            .render(120)
            .into_iter()
            .map(|line| pir::tui::ansi::strip_ansi(&line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rows.contains(&format!("mock-model • {expected}")),
            "resolved {resolved:?} settings {settings:?}: expected `mock-model • {expected}`, got:\n{rows}"
        );
        assert!(
            !rows.contains("thinking off"),
            "the footer must never paint `thinking off` behind a resolved level:\n{rows}"
        );
    }
}

/// The palette is process-global; serialize this module's rendering tests.
fn footer_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// PIR-BIN-078: `--thinking` applies WITH and WITHOUT `--model`
// ---------------------------------------------------------------------------
//
// The flag was OR-ed into `explicit` only inside the `Some(--model)` arm of
// the startup resolution, so a run without `--model` fell through to `None`
// and the settings default (then the shared `medium`) won while the same flag
// with `--model` did apply. Upstream applies `parsed.thinking`
// unconditionally (`main.ts:522-524`). These pins drive the two REAL resolution
// sites (the shared TUI/RPC resolver and the print host's own `run()`), so
// removing the hoist makes them fail.

fn print_settings(value: serde_json::Value) -> pir::config::settings::Settings {
    serde_json::from_value(value).expect("settings fixture")
}

/// Run ONE print invocation through the real host and return the reasoning
/// level every provider request carried.
async fn print_host_levels(
    settings: pir::config::settings::Settings,
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
) -> Vec<Option<ThinkingLevel>> {
    let dir = temp_dir("print-host");
    let model = bare_reasoning_model("mock");
    let seen: Arc<Mutex<Vec<Option<ThinkingLevel>>>> = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with(&model, Arc::clone(&seen));
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let config = pir::config::Config {
        session_dir: sessions.to_string_lossy().into_owned(),
        ..pir::config::Config::default()
    };
    let options = pir::coding_agent::print::PrintOptions {
        prompt: "hello".into(),
        additional_messages: Vec::new(),
        mode: pir::coding_agent::print::OutputMode::Json,
        session: pir::coding_agent::session_resolve::SessionPlan::New { id: None },
        no_context_files: true,
        cli_provider: cli_provider.map(str::to_owned),
        cli_model: cli_model.map(str::to_owned),
        cli_thinking,
        // The key rides the injected `PiEnvironment::api_keys` below; setting
        // the CLI override too would trip print's own
        // "--api-key requires a model" validation on the no-`--model` arms,
        // which is the case under test.
        api_key_override: None,
        model_patterns: None,
        session_name: None,
        system_prompt_override: Some("BASE".into()),
        append_system_prompt: Vec::new(),
        settings_default_tools: None,
        cli_tools: Vec::new(),
        cli_exclude_tools: Vec::new(),
        no_tools: true,
        no_builtin_tools: true,
        is_continuing: false,
        resources: pir::coding_agent::resources::ResourcePathOptions {
            no_skills: true,
            no_prompt_templates: true,
            ..pir::coding_agent::resources::ResourcePathOptions::default()
        },
        images: Vec::new(),
    };
    let outcome = pir::coding_agent::print::run_print_with_environment(
        options,
        &config,
        &[],
        dir.clone(),
        pir::ai::providers::PiEnvironment {
            registry: Arc::clone(&registry).as_ref().clone(),
            models: vec![model],
            api_keys: BTreeMap::from([("mock".to_owned(), "mock-key".to_owned())]),
            settings,
        },
    )
    .await
    .expect("run_print");
    assert_eq!(outcome.exit_code, 0);
    let levels = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let _ = std::fs::remove_dir_all(&dir);
    levels
}

/// The exact 078 contrast on the REAL print host: identical settings, model,
/// prompt; the only difference is `--model`. Before the hoist the
/// no-`--model` arm sent the settings default (`low`).
#[tokio::test(flavor = "current_thread")]
async fn print_host_applies_cli_thinking_with_and_without_an_explicit_model() {
    let settings = || print_settings(serde_json::json!({ "defaultThinkingLevel": "low" }));

    let without_model = print_host_levels(settings(), None, None, Some(ThinkingLevel::High)).await;
    assert_eq!(
        without_model,
        [Some(ThinkingLevel::High)],
        "`--thinking high` must beat the settings default even when `--model` is absent (078)"
    );

    let with_model = print_host_levels(
        settings(),
        None,
        Some("mock/mock-model"),
        Some(ThinkingLevel::High),
    )
    .await;
    assert_eq!(with_model, [Some(ThinkingLevel::High)]);
}

/// `--thinking off` without `--model` must also survive (it used to become the
/// settings default), and on the wire `off` emits no reasoning field on a
/// model whose map has no `off` entry.
#[tokio::test(flavor = "current_thread")]
async fn print_host_keeps_an_explicit_off_without_a_model() {
    let levels = print_host_levels(
        print_settings(serde_json::json!({ "defaultThinkingLevel": "high" })),
        None,
        None,
        Some(ThinkingLevel::Off),
    )
    .await;
    assert_eq!(
        levels,
        [Some(ThinkingLevel::Off)],
        "an explicit `--thinking off` must win over a settings default (078)"
    );
    let mut body = serde_json::json!({});
    pir::ai::api::openai::apply_reasoning_effort(
        &mut body,
        &bare_reasoning_model("mock"),
        levels[0],
    );
    assert!(
        body.get("reasoning_effort").is_none(),
        "`off` must not emit reasoning_effort: {body}"
    );
}

/// 074 and 078 together on the real host: `--thinking max` with NO `--model`
/// must win over the settings default AND clamp (`high`). This is the arm both
/// issues describe and neither the old hoist nor the old clamp covered.
#[tokio::test(flavor = "current_thread")]
async fn print_host_clamps_max_and_beats_the_settings_default_without_a_model() {
    let levels = print_host_levels(
        print_settings(serde_json::json!({ "defaultThinkingLevel": "low" })),
        None,
        None,
        Some(ThinkingLevel::Max),
    )
    .await;
    assert_eq!(
        levels,
        [Some(ThinkingLevel::High)],
        "`--thinking max` (no --model) must win over the settings default and clamp to the model's ladder"
    );

    // Settings default `max` (the lane-ab-config addendum arm) clamps too.
    let levels = print_host_levels(
        print_settings(serde_json::json!({ "defaultThinkingLevel": "max" })),
        None,
        Some("mock/mock-model"),
        None,
    )
    .await;
    assert_eq!(
        levels,
        [Some(ThinkingLevel::High)],
        "a settings-default `max` must clamp before the first request"
    );
}

/// The shared TUI/RPC resolver (`resolve_model`) hoists the flag and returns a
/// CLAMPED level — the footer seed the TUI paints before the worker's session
/// exists (PIR-TUI-032) is that same value.
#[test]
fn overrides_resolution_hoists_and_clamps_cli_thinking_without_a_model() {
    use pir::coding_agent::overrides::{resolve_model, CliModelOverrides};

    let model = bare_reasoning_model("mock");
    let api_keys = BTreeMap::from([("mock".to_owned(), "mock-key".to_owned())]);

    let selection = resolve_model(
        &CliModelOverrides {
            thinking: Some(ThinkingLevel::High),
            ..CliModelOverrides::default()
        },
        std::slice::from_ref(&model),
        &api_keys,
        &print_settings(serde_json::json!({ "defaultThinkingLevel": "low" })),
    )
    .expect("resolve");
    assert_eq!(
        selection.thinking,
        ThinkingLevel::High,
        "the TUI/RPC resolver must apply `--thinking` without `--model` (078)"
    );

    let selection = resolve_model(
        &CliModelOverrides {
            thinking: Some(ThinkingLevel::Max),
            ..CliModelOverrides::default()
        },
        std::slice::from_ref(&model),
        &api_keys,
        &print_settings(serde_json::json!({})),
    )
    .expect("resolve");
    assert_eq!(
        selection.thinking,
        ThinkingLevel::High,
        "the resolved level the footer and worker receive must be clamped (074/TUI-032)"
    );
}
