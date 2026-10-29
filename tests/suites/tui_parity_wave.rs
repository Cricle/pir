//! Parity-wave tests for the interactive-feature gap closure: the
//! backslash-Enter Submit-branch revert (upstream `editor.ts:1353-1362`),
//! the footer cache-hit-rate / (sub) / xp markers (footer.ts), the
//! `app.*` keybinding-id routing, the extension-dialog editing key map,
//! the selector fuzzy ordering (model-search.ts), the scoped-model /
//! default-model / default-thinking settings persistence, the `/reload`
//! hot-refresh seam, the dequeue queue drain, and the share trailing
//! entry.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::session_manager::SessionManager;
use pir::tui::app::{format_tokens, render_footer, FooterState, UsageTotals};
use pir::tui::command_dialogs::AuthProviderKind;
use pir::tui::dialogs::{dialog_key_from_crossterm, DialogBody, DialogKey, DialogOverlay};
use pir::tui::editor::EditorState;
use pir::tui::keybindings::{parse_key, Keybindings};
use pir::tui::selector::ModelSelector;
use pir::tui::terminal::{
    backslash_before_cursor, plain_enter_key, should_submit_on_backslash_enter,
};

// ---------------------------------------------------------------------------
// Fix 21: the Submit-branch backslash revert is unconditional
// ---------------------------------------------------------------------------

fn enter_key(shift: bool) -> KeyEvent {
    let mut modifiers = KeyModifiers::empty();
    if shift {
        modifiers.insert(KeyModifiers::SHIFT);
    }
    KeyEvent::new(KeyCode::Enter, modifiers)
}

fn backslash_state() -> EditorState {
    EditorState {
        lines: vec!["run\\".to_string()],
        cursor_line: 0,
        cursor_col: 4,
    }
}

#[test]
fn backslash_submit_branch_is_unconditional() {
    // Upstream editor.ts:1353-1362: the SUBMIT branch applies the
    // backslash → newline swap WITHOUT the hasShiftEnter gate (the gate
    // lives only in the newLine branch, `should_submit_on_backslash_enter`).
    let state = backslash_state();
    assert!(backslash_before_cursor(&state));
    assert!(plain_enter_key(&enter_key(false)));
    // The gate helper stays for the newLine branch and still refuses to
    // fire with the default bindings (submit = enter only).
    let defaults = Keybindings::new();
    assert!(!should_submit_on_backslash_enter(
        &defaults,
        &enter_key(false),
        &state
    ));
}

#[test]
fn backslash_helpers_reject_other_shapes() {
    assert!(!plain_enter_key(&enter_key(true)));
    assert!(!plain_enter_key(&KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::NONE
    )));
    let plain = EditorState {
        lines: vec!["run".to_string()],
        cursor_line: 0,
        cursor_col: 3,
    };
    assert!(!backslash_before_cursor(&plain));
}

// ---------------------------------------------------------------------------
// Fixes 1-2: footer cache-hit rate, (sub) and xp markers
// ---------------------------------------------------------------------------

#[test]
fn footer_renders_cache_hit_rate_when_cache_tokens_present() {
    let usage = UsageTotals {
        cache_read: 4000,
        cache_write: 1000,
        ..UsageTotals::default()
    };
    let state = FooterState {
        usage,
        cache_hit_rate: Some(80.0),
        model_id: "m".to_string(),
        ..FooterState::default()
    };
    let line = strip(&render_footer(120, &state)).join("\n");
    assert!(line.contains("CH80.0%"), "footer line: {line}");
}

#[test]
fn footer_hides_cache_hit_rate_without_cache_tokens() {
    let state = FooterState {
        cache_hit_rate: Some(80.0),
        model_id: "m".to_string(),
        ..FooterState::default()
    };
    let line = strip(&render_footer(120, &state)).join("\n");
    assert!(!line.contains("CH"), "footer line: {line}");
}

#[test]
fn footer_subscription_marker() {
    // Upstream footer.ts:189-196: a subscription-backed provider shows
    // `$0.000 (sub)` even with no accrued cost (kimi-coding is
    // subscription-backed despite API-key auth).
    let state = FooterState {
        model_id: "m".to_string(),
        model_provider: "kimi-coding".to_string(),
        ..FooterState::default()
    };
    let line = strip(&render_footer(120, &state)).join("\n");
    assert!(line.contains("(sub)"), "footer line: {line}");

    // A non-subscription provider with no cost shows neither.
    let plain = FooterState {
        model_id: "m".to_string(),
        model_provider: "openai".to_string(),
        ..FooterState::default()
    };
    let line = strip(&render_footer(120, &plain)).join("\n");
    assert!(!line.contains("(sub)"), "footer line: {line}");

    // The explicit subscription flag marks any provider.
    let flagged = FooterState {
        model_id: "m".to_string(),
        subscription: true,
        ..FooterState::default()
    };
    let line = strip(&render_footer(120, &flagged)).join("\n");
    assert!(line.contains("(sub)"), "footer line: {line}");
}

#[test]
fn footer_experimental_marker() {
    let state = FooterState {
        model_id: "m".to_string(),
        experimental: true,
        ..FooterState::default()
    };
    let line = strip(&render_footer(120, &state)).join("\n");
    assert!(line.contains("xp"), "footer line: {line}");
}

#[test]
fn footer_provider_prefix_only_when_multiple_providers() {
    let mut single = FooterState {
        model_id: "m".to_string(),
        model_provider: "openai".to_string(),
        available_providers: 1,
        ..FooterState::default()
    };
    let line = strip(&render_footer(200, &single)).join("\n");
    assert!(!line.contains("(openai)"), "footer line: {line}");
    single.available_providers = 2;
    let line = strip(&render_footer(200, &single)).join("\n");
    assert!(line.contains("(openai)"), "footer line: {line}");
}

#[test]
fn footer_routed_model_display() {
    let state = FooterState {
        model_id: "virtual".to_string(),
        model_reasoning: true,
        thinking_level: "high".to_string(),
        routed: Some(("physical-model".to_string(), Some("low".to_string()))),
        ..FooterState::default()
    };
    let line = strip(&render_footer(200, &state)).join("\n");
    assert!(
        line.contains("virtual • high → physical-model • low"),
        "footer line: {line}"
    );
}

fn strip(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| pir::tui::ansi::strip_ansi(line))
        .collect()
}

// ---------------------------------------------------------------------------
// Fix 20: keybinding id routing (tui.* + app.*)
// ---------------------------------------------------------------------------

#[test]
fn editor_ids_resolve_from_settings_overrides() {
    let bindings = Keybindings::from_settings(Some(&serde_json::json!({
        "tui.editor.cursorLineStart": "ctrl+q",
        "tui.editor.deleteToLineEnd": ["ctrl+q", "ctrl+k"]
    })))
    .expect("settings parse");
    let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert!(bindings.matches_id("tui.editor.cursorLineStart", &ctrl_q));
    assert!(bindings.matches_id("tui.editor.deleteToLineEnd", &ctrl_q));
    // The default chord (home) no longer matches the overridden id.
    let home = KeyEvent::new(KeyCode::Home, KeyModifiers::NONE);
    assert!(!bindings.matches_id("tui.editor.cursorLineStart", &home));
    // An untouched id keeps its upstream default (`end` for cursorLineEnd).
    let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
    assert!(bindings.matches_id("tui.editor.cursorLineEnd", &end));
}

#[test]
fn app_ids_resolve_with_defaults_and_overrides() {
    let defaults = Keybindings::new();
    let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
    // Upstream-verbatim default (core/keybindings.ts:59-62).
    assert!(defaults.matches_app_id("app.editor.external", &ctrl_g));
    assert_eq!(defaults.app_id_display("app.editor.external"), "ctrl+g");
    let ctrl_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
    assert!(!defaults.matches_app_id("app.message.dequeue", &ctrl_v));
    // Windows default alt+q, other platforms alt+up (keybindings.ts:71-74).
    if pir::config::keybindings::use_windows_keybindings() {
        let alt_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT);
        assert!(defaults.matches_app_id("app.message.dequeue", &alt_q));
    } else {
        let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        assert!(defaults.matches_app_id("app.message.dequeue", &alt_up));
    }

    // A settings override moves the chord.
    let overrides = Keybindings::from_settings(Some(&serde_json::json!({
        "app.editor.external": "ctrl+shift+e"
    })))
    .expect("settings parse");
    assert!(!overrides.matches_app_id("app.editor.external", &ctrl_g));
    let chord = KeyEvent::new(
        KeyCode::Char('e'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(overrides.matches_app_id("app.editor.external", &chord));

    // Unregistered ids never match.
    assert!(defaults.keys_for_app_id("app.nonexistent").is_none());
}

#[test]
fn parse_key_matches_id_grammar() {
    let combo = parse_key("ctrl+shift+p").expect("parse");
    let key = KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(combo.matches(&key));
}

// ---------------------------------------------------------------------------
// Fix 19: extension dialog editing key map
// ---------------------------------------------------------------------------

#[test]
fn input_dialog_supports_the_full_editing_surface() {
    let mut overlay = DialogOverlay::new(
        1,
        DialogBody::Input {
            title: "t".to_string(),
            placeholder: String::new(),
            default_value: "hello world".to_string(),
        },
    );
    // End jumps to the tail; ctrl+w kills the trailing word.
    overlay.handle_key(DialogKey::End);
    overlay.handle_key(DialogKey::DeleteWordBackward);
    assert_eq!(overlay.text().as_deref(), Some("hello "));
    // Yank the kill back (ctrl+y).
    overlay.handle_key(DialogKey::Yank);
    assert_eq!(overlay.text().as_deref(), Some("hello world"));
    // Word-left, then delete-to-line-end (ctrl+k) trims the trailing word.
    overlay.handle_key(DialogKey::WordLeft);
    overlay.handle_key(DialogKey::DeleteToLineEnd);
    assert_eq!(overlay.text().as_deref(), Some("hello "));
    // Enter still submits, Esc still cancels.
    assert!(matches!(
        overlay.handle_key(DialogKey::Enter),
        pir::tui::dialogs::DialogOutcome::Submit(_)
    ));

    let mut cancel = DialogOverlay::new(
        2,
        DialogBody::Input {
            title: "t".to_string(),
            placeholder: String::new(),
            default_value: String::new(),
        },
    );
    assert!(matches!(
        cancel.handle_key(DialogKey::Esc),
        pir::tui::dialogs::DialogOutcome::Cancel
    ));
}

#[test]
fn dialog_key_mapping_covers_the_editor_chords() {
    let plain = KeyModifiers::NONE;
    let ctrl = KeyModifiers::CONTROL;
    let alt = KeyModifiers::ALT;
    let key = |code: KeyCode, modifiers: KeyModifiers| {
        dialog_key_from_crossterm(code, modifiers).expect("mapped")
    };
    assert_eq!(key(KeyCode::Home, plain), DialogKey::Home);
    assert_eq!(key(KeyCode::End, plain), DialogKey::End);
    assert_eq!(key(KeyCode::Delete, plain), DialogKey::Delete);
    assert_eq!(key(KeyCode::Left, ctrl), DialogKey::WordLeft);
    assert_eq!(key(KeyCode::Right, alt), DialogKey::WordRight);
    assert_eq!(key(KeyCode::Char('w'), ctrl), DialogKey::DeleteWordBackward);
    assert_eq!(key(KeyCode::Char('d'), alt), DialogKey::DeleteWordForward);
    assert_eq!(key(KeyCode::Char('u'), ctrl), DialogKey::DeleteToLineStart);
    assert_eq!(key(KeyCode::Char('k'), ctrl), DialogKey::DeleteToLineEnd);
    assert_eq!(key(KeyCode::Char('y'), ctrl), DialogKey::Yank);
    assert_eq!(key(KeyCode::Char('y'), alt), DialogKey::YankPop);
    assert_eq!(key(KeyCode::Char('-'), ctrl), DialogKey::Undo);
    assert_eq!(key(KeyCode::Char('j'), ctrl), DialogKey::Newline);
    assert_eq!(key(KeyCode::Char('c'), ctrl), DialogKey::Esc);
    // A shifted char stays text input.
    assert_eq!(
        key(KeyCode::Char('X'), KeyModifiers::SHIFT),
        DialogKey::Char('X')
    );
}

// ---------------------------------------------------------------------------
// Fix 16: selector fuzzy ordering (model-search.ts)
// ---------------------------------------------------------------------------

#[test]
fn model_selector_prefers_the_provider_prefix_match() {
    // Upstream getModelSelectorSearchText ranks the provider-prefixed query
    // ahead of proxy-provider ids like openrouter/openai/gpt-5.
    let models = vec![
        ("openrouter".to_string(), "gpt-5".to_string()),
        ("openai".to_string(), "gpt-5".to_string()),
    ];
    let selector = ModelSelector::new(&models, None, None);
    selector.set_query("openai");
    assert_eq!(
        selector.selected().as_deref(),
        Some("openai/gpt-5"),
        "the exact provider match must win"
    );
}

#[test]
fn model_selector_fuzzy_ranks_best_score_first() {
    let models = vec![
        ("openai".to_string(), "gpt-4o".to_string()),
        ("openai".to_string(), "gpt-5".to_string()),
    ];
    let selector = ModelSelector::new(&models, None, None);
    selector.set_query("gpt-5");
    assert_eq!(selector.selected().as_deref(), Some("openai/gpt-5"));
}

// ---------------------------------------------------------------------------
// Fixes 7 + 11: settings persistence (scoped models, default model,
// default thinking level)
// ---------------------------------------------------------------------------

fn temp_settings_dir(label: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pir-parity-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&path).expect("create temp dir");
    std::fs::create_dir_all(path.join(".pi")).expect("create project dir");
    path
}

#[test]
fn scoped_models_persist_into_settings() {
    // Upstream `onPersist` (interactive-mode.ts:5438-5446): the confirmation
    // writes `settingsManager.setEnabledModels` and reports "Model
    // selection saved to settings". pir persists the enabled-model list
    // through the global settings store.
    let temp = temp_settings_dir("scoped-models");
    let mut manager = pir::config::settings::SettingsManager::load(
        &temp.join(".pi").join("agent"),
        &temp.join(".pi"),
        &temp,
        true,
    );
    manager.drain_errors();
    manager.set_global_field(
        "enabledModels",
        serde_json::json!(["openai/gpt-5", "anthropic/claude-x"]),
    );
    assert!(manager.drain_errors().is_empty(), "persist must succeed");
    let reread = pir::config::settings::SettingsManager::load(
        &temp.join(".pi").join("agent"),
        &temp.join(".pi"),
        &temp,
        true,
    );
    assert_eq!(
        reread.settings.enabled_models.as_deref(),
        Some(&["openai/gpt-5".to_string(), "anthropic/claude-x".to_string()][..])
    );
}

#[test]
fn default_model_and_thinking_persist_into_settings() {
    // Upstream `selectModel(model, true)` → `setModel(model, { persist:
    // true })` and the thinking selector's `onSelectAsDefault`.
    let temp = temp_settings_dir("defaults");
    let mut manager = pir::config::settings::SettingsManager::load(
        &temp.join(".pi").join("agent"),
        &temp.join(".pi"),
        &temp,
        true,
    );
    manager.drain_errors();
    manager.set_global_field("defaultProvider", serde_json::json!("openai"));
    manager.set_global_field("defaultModel", serde_json::json!("gpt-5"));
    manager.set_global_field("defaultThinkingLevel", serde_json::json!("high"));
    assert!(manager.drain_errors().is_empty());
    let reread = pir::config::settings::SettingsManager::load(
        &temp.join(".pi").join("agent"),
        &temp.join(".pi"),
        &temp,
        true,
    );
    assert_eq!(reread.settings.default_provider.as_deref(), Some("openai"));
    assert_eq!(reread.settings.default_model.as_deref(), Some("gpt-5"));
    assert_eq!(
        reread.settings.default_thinking_level,
        Some(pir::ai::types::ThinkingLevel::High)
    );
}

// ---------------------------------------------------------------------------
// Fix 12: shift+tab capability clamp (cycle_thinking_level)
// ---------------------------------------------------------------------------

fn reasoning_model_with_map() -> Model {
    let mut model = fake_model();
    model.reasoning = true;
    model.thinking_level_map = Some({
        let mut map = std::collections::BTreeMap::new();
        for level in ["off", "minimal", "low", "medium", "high"] {
            map.insert(level.to_string(), Some("0".to_string()));
        }
        // `null` marks unsupported; xhigh/max absent (unsupported).
        map.insert("xhigh".to_string(), None);
        map
    });
    model
}

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
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

type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

fn scripted(respond: Responder) -> Arc<ProviderRegistry> {
    struct FakeProvider {
        calls: AtomicUsize,
        respond: Responder,
    }
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
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
        }
        fn stream(
            &self,
            _model: &Model,
            context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            let message = (self.respond)(index, &context);
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        }
    }
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

fn assistant(blocks: Vec<ContentBlock>, reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: blocks,
        api: "fake-api".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        usage: Usage::default(),
        stop_reason: reason,
        timestamp: 0,
        ..AssistantMessage::default()
    }
}

fn text(t: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: t.to_string(),
        text_signature: None,
    })
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    temp_settings_dir(label)
}

#[tokio::test]
async fn thinking_cycle_clamps_to_model_supported_levels() {
    let temp = temp_dir("thinking-clamp");
    let session = SessionManager::create(".", &temp).expect("create session");
    let registry = scripted(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let mut model = reasoning_model_with_map();
    model.provider = "fake".into();
    model.id = "fake-model".into();
    let agent = pir::agent_session::AgentSession::new(pir::agent_session::AgentSessionConfig {
        provider_registry: registry,
        model: model.clone(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });
    // The supported ladder stops at `high` (xhigh mapped to null, max
    // unnamed) — cycling from `off` walks only the supported levels.
    let levels = agent.available_thinking_levels();
    let keys: Vec<&str> = levels.iter().map(|level| level.wire_key()).collect();
    assert_eq!(keys, vec!["off", "minimal", "low", "medium", "high"]);

    let first = agent.cycle_thinking_level().await.expect("reasoning model");
    assert_eq!(first.wire_key(), "minimal");
    for expected in ["low", "medium", "high", "off", "minimal"] {
        let next = agent.cycle_thinking_level().await.expect("cycle");
        assert_eq!(next.wire_key(), expected, "capability clamp order");
    }

    // A non-reasoning model never cycles (upstream: "Current model does not
    // support thinking").
    let session = SessionManager::create(".", &temp).expect("create session");
    let registry = scripted(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let plain = pir::agent_session::AgentSession::new(pir::agent_session::AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });
    assert!(plain.cycle_thinking_level().await.is_none());
}

// ---------------------------------------------------------------------------
// Fix 8: the /reload hot-refresh seam
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prompt_resources_hot_refresh_swaps_in_place() {
    let temp = temp_dir("hot-refresh");
    let session = SessionManager::create(".", &temp).expect("create session");
    let registry = scripted(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    // The skills section only renders when a reader tool is declared
    // (`src/system_prompt.rs`), so the registry must hold `read` for the
    // hot-swap to be observable in the prompt at all.
    let mut tools = pir::agent::tool_execution::ToolRegistry::new();
    pir::coding_agent::register_core_tools(&mut tools);
    let agent = Arc::new(pir::agent_session::AgentSession::new(
        pir::agent_session::AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session: Some(session),
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: Some(temp.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
    ));
    // The Arc-safe seam (`/reload` drives this through the worker) swaps
    // the discovered resources of the RUNNING session: installing a skill
    // rebuilds the context-derived prompt with its skills section.
    let skills_first = vec![pir::skills::SkillInfo {
        name: "hot-swap-marker".to_string(),
        description: "Proves the reload rebuilt the skills section".to_string(),
        path: temp.join("SKILL.md"),
        disable_model_invocation: false,
    }];
    agent.set_prompt_resources_shared(skills_first, Vec::new());
    let installed = agent
        .system_prompt_snapshot()
        .expect("installing a skill must rebuild the context-derived prompt");
    assert!(
        installed.contains("hot-swap-marker"),
        "the rebuilt prompt must render the installed skill: {installed}"
    );
    // A second call replaces the resources: with no skills left the rebuilt
    // prompt must no longer carry the old skill's section.
    agent.set_prompt_resources_shared(Vec::new(), Vec::new());
    let reloaded = agent.system_prompt_snapshot().unwrap_or_default();
    assert!(
        !reloaded.contains("hot-swap-marker"),
        "the empty reload must drop the previously installed skill: {reloaded}"
    );
}

// ---------------------------------------------------------------------------
// Fix 6: the dequeue queue drain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dequeue_drains_both_queues() {
    let temp = temp_dir("dequeue");
    let session = SessionManager::create(".", &temp).expect("create session");
    let registry = scripted(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let agent = Arc::new(pir::agent_session::AgentSession::new(
        pir::agent_session::AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: pir::agent::tool_execution::ToolRegistry::new(),
            session: Some(session),
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: Some(temp.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
    ));
    // Queue one steering and one follow-up message while idle, then drain
    // (upstream `clearAllQueues` inside `restoreQueuedMessagesToEditor`).
    agent.steer("first");
    agent.follow_up("second");
    let cleared = agent.clear_queue().await;
    assert_eq!(cleared.steering, vec!["first".to_string()]);
    assert_eq!(cleared.follow_up, vec!["second".to_string()]);
    // A second drain is empty.
    let cleared = agent.clear_queue().await;
    assert!(cleared.steering.is_empty() && cleared.follow_up.is_empty());
}

// ---------------------------------------------------------------------------
// Fix 17: the /share trailing entry
// ---------------------------------------------------------------------------

#[test]
fn share_trailing_entry_carries_metadata() {
    let entry = pir::tui::slash::share_trailing_entry(
        Some("system prompt"),
        &[(
            "bash".to_string(),
            "run commands".to_string(),
            Some(serde_json::json!({"type": "object"})),
        )],
    );
    assert_eq!(entry["customType"], "pi.share");
    assert_eq!(entry["data"]["systemPrompt"], "system prompt");
    assert_eq!(entry["data"]["tools"][0]["name"], "bash");
    assert_eq!(entry["data"]["tools"][0]["parameters"]["type"], "object");
    assert!(entry.get("id").is_some());
    assert!(entry.get("timestamp").is_some());
}

#[test]
fn serialize_session_branch_appends_the_trailing_entry() {
    let temp = temp_dir("share-trailing");
    let manager = SessionManager::create(&temp.to_string_lossy(), &temp).expect("create");
    let trailing = [pir::tui::slash::share_trailing_entry(Some("prompt"), &[])];
    let output = pir::tui::slash::serialize_session_branch_with_trailing(&manager, &trailing);
    let lines: Vec<&str> = output.lines().collect();
    assert!(!lines.is_empty());
    let last: serde_json::Value = serde_json::from_str(lines[lines.len() - 1]).expect("jsonl line");
    assert_eq!(last["customType"], "pi.share");
    assert_eq!(last["data"]["systemPrompt"], "prompt");
    // The trailing entry chains onto the previous line's parentId (or the
    // empty branch's null).
    assert!(last.get("parentId").is_some());
}

// ---------------------------------------------------------------------------
// Fix 18: the bug-report bundle helpers
// ---------------------------------------------------------------------------

#[test]
fn bug_report_redacts_sensitive_urls_and_keys() {
    use pir::coding_agent::bug_report::{redact_json_value, redact_url};
    assert_eq!(
        redact_url("https://user:pass@example.com/x?api_key=abc&safe=1"),
        "https://example.com/x?api_key=<redacted>&safe=1"
    );
    let redacted = redact_json_value(&serde_json::json!({
        "apiKey": "secret",
        "nested": { "authToken": "t", "url": "https://u:p@h/x?token=z" }
    }));
    assert_eq!(redacted["apiKey"], "<redacted>");
    assert_eq!(redacted["nested"]["authToken"], "<redacted>");
    assert_eq!(redacted["nested"]["url"], "https://h/x?token=<redacted>");
}

#[test]
fn bug_report_files_shape() {
    let bundle = pir::coding_agent::bug_report::BugReportBundle {
        metadata: serde_json::json!({"schemaVersion": 1}),
        diagnostics: serde_json::json!({"assistant": []}),
        session_jsonl: Some("{\"type\":\"session\"}\n".to_string()),
        hint: Some("crash".to_string()),
        summary: None,
    };
    let files = pir::coding_agent::bug_report::bug_report_files(&bundle);
    // Upstream order (core/bug-report.ts:252-262): report.json,
    // diagnostics.json, then the optional session.jsonl / summary.md.
    assert_eq!(files[0].name, "report.json");
    assert_eq!(files[0].content_type, "application/json");
    assert_eq!(files[1].name, "diagnostics.json");
    assert_eq!(files[1].content_type, "application/json");
    assert_eq!(files[2].name, "session.jsonl");
    assert_eq!(files[2].content_type, "application/x-ndjson");
    // A summary.md attaches only when the transcript is declined.
    let no_session = pir::coding_agent::bug_report::BugReportBundle {
        metadata: serde_json::json!({"schemaVersion": 1}),
        diagnostics: serde_json::json!({"assistant": []}),
        session_jsonl: None,
        hint: Some("hangs on startup".to_string()),
        summary: Some("# Bug report summary\n\nhangs on startup".to_string()),
    };
    let files = pir::coding_agent::bug_report::bug_report_files(&no_session);
    assert_eq!(files.len(), 3);
    assert_eq!(files[2].name, "summary.md");
    assert_eq!(files[2].content_type, "text/markdown");
    assert!(
        files[2].data.ends_with('\n'),
        "summary.md ends with a newline"
    );
}

// ---------------------------------------------------------------------------
// Fix 9: the /login provider rows carry the auth kind
// ---------------------------------------------------------------------------

#[test]
fn auth_provider_rows_carry_the_login_method() {
    let oauth = AuthProviderKind::OAuth;
    let api_key = AuthProviderKind::ApiKey;
    assert_eq!(oauth.as_str(), "oauth");
    assert_eq!(api_key.as_str(), "api_key");
    assert_ne!(oauth, api_key);
}

// ---------------------------------------------------------------------------
// format_tokens sanity (footer dependency)
// ---------------------------------------------------------------------------

#[test]
fn format_tokens_matches_upstream_rounding() {
    assert_eq!(format_tokens(999), "999");
    assert_eq!(format_tokens(4_000_000), "4.0M");
}
