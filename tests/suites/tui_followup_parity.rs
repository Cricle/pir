//! TUI parity follow-ups (MAPPING.md "Open follow-ups", closed 2026-10-12):
//!
//! - model selector Tab scope toggle (upstream `model-selector.ts:363-370`),
//! - `/scoped-models` live in-selector catalog refresh (upstream
//!   `interactive-mode.ts:5390-5500`),
//! - `/tree` Esc-during-summary abort (`agent-session.ts:4043-4060`,
//!   `interactive-mode.ts:5652-5691`),
//! - transport retry countdown indicator (`status-indicator.ts:51-69`).
//!
//! Pure-seam/rendering tests: no real terminal is built.

use pir::tui::ansi::strip_ansi;
use pir::tui::core::Component;
use pir::tui::selector::{ModelScope, ModelSelector};
use pir::tui::slash::EnabledModels;
use pir::tui::terminal::{
    merged_catalog_pairs, scoped_models_refresh_pending, scoped_models_refresh_status,
    ScopedModelsRefreshKind, ScopedModelsSelector,
};
use pir::tui::theme::{set_theme, with_theme, ThemeColor};

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(provider, id)| ((*provider).to_string(), (*id).to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// Model selector Tab scope toggle (upstream model-selector.ts)
// ---------------------------------------------------------------------------

#[test]
fn model_selector_starts_scoped_when_a_scope_is_configured() {
    // The process-global theme: serialize with the sibling suites' swaps.
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let all = pairs(&[
        ("openai", "gpt-5.2"),
        ("anthropic", "opus-4.5"),
        ("openrouter", "llama"),
    ]);
    let scoped = pairs(&[("anthropic", "opus-4.5")]);
    let selector = ModelSelector::new(&all, Some("anthropic/opus-4.5"), None);
    selector.set_scoped_models(&scoped);

    // Upstream constructor: `scope = scopedModels.length > 0 ? "scoped" : "all"`
    // (model-selector.ts:94).
    assert_eq!(selector.scope(), ModelScope::Scoped);
    assert!(selector.scope_available());

    let rendered = selector.render(60).join("\n");
    assert!(
        rendered.contains(&with_theme(|theme| theme.fg(ThemeColor::Accent, "scoped"))),
        "the active scope is accent-colored: {rendered}"
    );
    assert!(
        !rendered.contains(&with_theme(|theme| theme.fg(ThemeColor::Accent, "all"))),
        "the inactive scope stays muted: {rendered}"
    );
    let plain = strip_ansi(&rendered);
    assert!(!plain.contains("Use /login to add providers"));
    assert!(plain.contains("opus-4.5"));
    assert!(
        !plain.contains("gpt-5.2"),
        "the scoped catalog lists only the scoped models: {plain}"
    );
}

#[test]
fn model_selector_tab_toggles_scope_and_swaps_the_catalog() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let all = pairs(&[("openai", "gpt-5.2"), ("anthropic", "opus-4.5")]);
    let scoped = pairs(&[("anthropic", "opus-4.5")]);
    let selector = ModelSelector::new(&all, Some("anthropic/opus-4.5"), None);
    selector.set_scoped_models(&scoped);

    // Tab → `all`.
    assert!(selector.toggle_scope());
    assert_eq!(selector.scope(), ModelScope::All);
    let rendered = selector.render(60).join("\n");
    assert!(rendered.contains(&with_theme(|theme| theme.fg(ThemeColor::Accent, "all"))));
    assert!(!rendered.contains(&with_theme(|theme| theme.fg(ThemeColor::Accent, "scoped"))));
    // The widened catalog exposes models outside the scope.
    selector.set_query("gpt-5.2");
    assert_eq!(selector.selected().as_deref(), Some("openai/gpt-5.2"));

    // Tab → back to `scoped`.
    assert!(selector.toggle_scope());
    assert_eq!(selector.scope(), ModelScope::Scoped);
    selector.set_query("gpt-5.2");
    assert_eq!(
        selector.selected(),
        None,
        "the scoped catalog must not match a model outside the scope"
    );
}

#[test]
fn model_selector_scope_toggle_without_a_scope_is_inert() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let selector = ModelSelector::new(&[], None, None);
    assert!(!selector.scope_available());
    assert!(!selector.toggle_scope());
    assert_eq!(selector.scope(), ModelScope::All);
    let plain = strip_ansi(&selector.render(120).join("\n"));
    assert!(plain
        .contains("Only showing models from configured providers. Use /login to add providers."));
    assert!(!plain.contains("Scope:"));
}

#[test]
fn model_selector_scope_switch_keeps_the_current_model_when_present() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let all = pairs(&[
        ("openai", "gpt-5.2"),
        ("anthropic", "opus-4.5"),
        ("openrouter", "llama"),
    ]);
    let scoped = pairs(&[("openrouter", "llama")]);
    let selector = ModelSelector::new(&all, Some("openrouter/llama"), None);
    selector.set_scoped_models(&scoped);
    assert_eq!(selector.selected().as_deref(), Some("openrouter/llama"));

    // Upstream `setScope`: the selection follows the current model into the
    // new catalog (`currentIndex >= 0 ? currentIndex : 0`).
    selector.toggle_scope();
    assert_eq!(selector.scope(), ModelScope::All);
    assert_eq!(selector.selected().as_deref(), Some("openrouter/llama"));

    // A current model outside the scope falls back to the first row.
    let selector = ModelSelector::new(&all, Some("openai/gpt-5.2"), None);
    selector.set_scoped_models(&scoped);
    assert_eq!(selector.selected().as_deref(), Some("openrouter/llama"));
}

// ---------------------------------------------------------------------------
// /scoped-models live in-selector catalog refresh (upstream interactive-mode)
// ---------------------------------------------------------------------------

#[test]
fn scoped_models_selector_refreshes_its_catalog_in_place() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let selector = ScopedModelsSelector::new(
        &pairs(&[("openai", "gpt-5.2"), ("anthropic", "opus-4.5")]),
        EnabledModels::all(),
    );
    selector.move_down();
    assert_eq!(
        selector.selected_ref().as_deref(),
        Some("anthropic/opus-4.5")
    );

    // The selector opens on the cached catalog with the pending status
    // (upstream `refreshStatus: "Refreshing model catalogs…"`).
    let (pending, kind) = scoped_models_refresh_pending();
    selector.set_refresh_status(pending, kind);
    assert!(strip_ansi(&selector.render(60).join("\n")).contains("Refreshing model catalogs…"));

    // Live reply: the OPEN selector's catalog grows in place and the
    // highlighted row follows the same ref (upstream `updateModels`).
    selector.update_models(
        &pairs(&[
            ("anthropic", "opus-4.5"),
            ("openai", "gpt-5.2"),
            ("openai", "gpt-6"),
        ]),
        None,
    );
    assert_eq!(selector.refs().len(), 3);
    assert_eq!(
        selector.selected_ref().as_deref(),
        Some("anthropic/opus-4.5")
    );

    let (message, kind) = scoped_models_refresh_status(None, false);
    selector.set_refresh_status(message, kind);
    let rendered = selector.render(60).join("\n");
    let plain = strip_ansi(&rendered);
    assert!(plain.contains("openai/gpt-6"), "live catalog row: {plain}");
    assert!(plain.contains("Model catalogs refreshed."));
    assert!(rendered.contains(&with_theme(|theme| {
        theme.fg(ThemeColor::Success, " Model catalogs refreshed.")
    })));
}

#[test]
fn scoped_models_refresh_status_matches_upstream_texts() {
    assert_eq!(
        scoped_models_refresh_pending(),
        (
            "Refreshing model catalogs…".to_string(),
            ScopedModelsRefreshKind::Pending
        )
    );
    assert_eq!(
        scoped_models_refresh_status(None, false),
        (
            "Model catalogs refreshed.".to_string(),
            ScopedModelsRefreshKind::Success
        )
    );
    assert_eq!(
        scoped_models_refresh_status(None, true),
        (
            "Model refresh timed out; showing cached models.".to_string(),
            ScopedModelsRefreshKind::Warning
        )
    );
    assert_eq!(
        scoped_models_refresh_status(Some("openai: HTTP 500"), false),
        (
            "Could not refresh model catalogs: openai: HTTP 500".to_string(),
            ScopedModelsRefreshKind::Warning
        )
    );
}

#[test]
fn merged_catalog_pairs_appends_the_persisted_overlay_ids() {
    let registry = pairs(&[("openai", "gpt-5.2")]);
    let store = vec![
        (
            "openai".to_string(),
            vec!["gpt-5.2".to_string(), "gpt-6".to_string()],
        ),
        ("qwen".to_string(), vec!["qwen3".to_string()]),
    ];
    assert_eq!(
        merged_catalog_pairs(&registry, &store),
        pairs(&[
            ("openai", "gpt-5.2"),
            ("openai", "gpt-6"),
            ("qwen", "qwen3")
        ])
    );
}

// ---------------------------------------------------------------------------
// /tree Esc-during-summary abort: the re-opened tree restores the selection
// (upstream `showTreeSelector(initialSelectedId)`, interactive-mode.ts:5679)
// ---------------------------------------------------------------------------

#[test]
fn tree_selector_select_id_restores_the_aborted_summary_selection() {
    use pir::tui::selector::{TreeEntry, TreeEntryKind, TreeFilterMode, TreeSelector};

    let entry = |id: &str, display: &str, kind: TreeEntryKind| TreeEntry::root(id, display, kind);
    let entries = vec![
        entry("a", "first", TreeEntryKind::User),
        entry("b", "second", TreeEntryKind::Assistant),
    ];
    let selector = TreeSelector::new(&entries, TreeFilterMode::Default);
    assert_eq!(selector.selected().as_deref(), Some("a"));

    // The aborted summarization re-opens the tree on the entry the user had
    // chosen.
    assert!(selector.select_id("b"));
    assert_eq!(selector.selected().as_deref(), Some("b"));

    // An entry hidden by the active filter keeps the previous selection.
    selector.set_filter_mode(TreeFilterMode::UserOnly);
    assert!(!selector.select_id("b"));
    assert_eq!(selector.selected().as_deref(), Some("a"));
    assert!(!selector.select_id("missing"));
    assert_eq!(selector.selected().as_deref(), Some("a"));
}

/// `/tree` real hierarchy (audit §7 #8): rows indent by depth and
/// `app.tree.foldOrUp` / `app.tree.unfoldOrDown` collapse and expand
/// subtrees. The flat-list behavior is preserved for a flat input.
#[test]
fn tree_selector_indents_and_folds() {
    use pir::tui::core::Component;
    use pir::tui::selector::{TreeEntry, TreeEntryKind, TreeFilterMode, TreeSelector};

    let entries = vec![
        TreeEntry {
            id: "a".to_string(),
            display: "root".to_string(),
            label: None,
            label_timestamp: None,
            kind: TreeEntryKind::User,
            depth: 0,
            parent_id: None,
        },
        TreeEntry {
            id: "b".to_string(),
            display: "child".to_string(),
            label: None,
            label_timestamp: None,
            kind: TreeEntryKind::Assistant,
            depth: 1,
            parent_id: Some("a".to_string()),
        },
        TreeEntry {
            id: "c".to_string(),
            display: "grand".to_string(),
            label: None,
            label_timestamp: None,
            kind: TreeEntryKind::Tool,
            depth: 2,
            parent_id: Some("b".to_string()),
        },
        TreeEntry {
            id: "d".to_string(),
            display: "sibling".to_string(),
            label: None,
            label_timestamp: None,
            kind: TreeEntryKind::Assistant,
            depth: 1,
            parent_id: Some("a".to_string()),
        },
    ];
    let selector = TreeSelector::new(&entries, TreeFilterMode::Default);
    assert_eq!(selector.visible_len(), 4);

    // Indentation + fold glyphs: root 0, child 2, grandchild 4 cells.
    let lines = selector.render(80);
    assert!(lines[3].contains("▾ root"), "root row: {}", lines[3]);
    assert!(lines[4].contains("  ▾ child"), "child row: {}", lines[4]);
    assert!(lines[5].contains("    "), "grandchild row: {}", lines[5]);
    assert!(lines[6].contains("  sibling"), "sibling row: {}", lines[6]);

    // Collapsing the root hides its whole subtree; the selection survives.
    assert_eq!(selector.selected().as_deref(), Some("a"));
    selector.fold_or_up();
    assert!(selector.is_collapsed("a"));
    assert_eq!(selector.visible_len(), 1);
    let lines = selector.render(80);
    assert!(lines[3].contains("▸ root"), "collapsed root: {}", lines[3]);

    // Unfolding restores every row.
    selector.unfold_or_down();
    assert!(!selector.is_collapsed("a"));
    assert_eq!(selector.visible_len(), 4);

    // Fold on a leaf moves up instead (upstream treeFoldOrUp).
    assert!(selector.select_id("c"));
    selector.fold_or_up();
    assert_eq!(selector.selected().as_deref(), Some("b"));

    // A collapsed entry is hidden by an ancestor fold even when its own
    // subtree is expanded, and the filter mode still applies to folded rows.
    selector.set_collapsed("a", true);
    assert_eq!(selector.visible_len(), 1);
    selector.set_collapsed("a", false);
    selector.set_filter_mode(TreeFilterMode::UserOnly);
    assert_eq!(selector.visible_len(), 1);
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Transport retry countdown (upstream `RetryStatusIndicator` +
// `auto_retry_start/end`): the session's retry wait reaches the host sink and
// `abort_retry()` (Esc) cancels it.
// ---------------------------------------------------------------------------

/// Serve one `429` (with `retry-after-ms`) per request on `stream`, counting
/// the requests; the connection stays alive so a pooled retry reuses it.
fn serve_retryable_429(
    mut stream: std::net::TcpStream,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    retry_after_ms: u64,
) {
    use std::io::{BufRead, BufReader, Read, Write};
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(clone);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            break;
        }
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                break;
            }
            if header.trim().is_empty() {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        if content_length > 0 {
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);
        }
        requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let body = br#"{"error":{"message":"rate limited"}}"#;
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after-ms: {retry_after_ms}\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );
        if stream.write_all(response.as_bytes()).is_err() {
            break;
        }
        if stream.write_all(body).is_err() {
            break;
        }
        let _ = stream.flush();
    }
}

fn retryable_429_endpoint(
    retry_after_ms: u64,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let counter = std::sync::Arc::clone(&counter);
            std::thread::spawn(move || serve_retryable_429(stream, counter, retry_after_ms));
        }
    });
    (format!("http://{addr}/v1"), requests)
}

/// The OpenAI-compatible provider + session model pointed at `base`, with a
/// `max_retries` budget on the client.
fn retry_session(
    base: &str,
    max_retries: u32,
    max_retry_delay: std::time::Duration,
) -> std::sync::Arc<pir::agent_session::AgentSession> {
    use pir::ai::types::*;
    use std::sync::Arc;

    let model = Model {
        id: "retry-test-model".into(),
        name: "Retry test model".into(),
        api: "openai-chat".into(),
        provider: "retrytest".into(),
        base_url: base.to_string(),
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
        max_tokens: 512,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    };
    let client =
        pir::ai::transport::ProviderRequestClient::new(pir::ai::transport::ProviderRequestConfig {
            timeout: std::time::Duration::from_secs(5),
            max_retries,
            max_retry_delay,
            ..pir::ai::transport::ProviderRequestConfig::default()
        })
        .expect("client builds");
    let provider = Arc::new(
        pir::ai::api::openai::OpenAiCompatibleProvider::new("retrytest", base, vec![model.clone()])
            .expect("provider builds")
            .with_client(client),
    );
    let mut registry = pir::ai::provider::ProviderRegistry::default();
    registry.set_provider(provider);

    Arc::new(pir::agent_session::AgentSession::new(
        pir::agent_session::AgentSessionConfig {
            provider_registry: Arc::new(registry),
            model,
            api_key: Some("test-key".into()),
            thinking: ThinkingLevel::Off,
            tools: pir::agent::tool_execution::ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 2,
            system_prompt: None,
            cwd: Some(std::env::temp_dir()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
    ))
}

#[tokio::test]
async fn session_retry_countdown_reaches_the_host_sink_and_settles() {
    use pir::agent_session::RetryEvent;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    let (base, requests) = retryable_429_endpoint(1);
    let agent = retry_session(&base, 1, std::time::Duration::from_secs(1));
    let events: Arc<Mutex<Vec<RetryEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&events);
    agent.set_host_retry_notice(Arc::new(move |event| recorder.lock().unwrap().push(event)));

    let message = agent
        .prompt("hello", Arc::new(|_event| {}))
        .await
        .expect("prompt runs");
    // The retry budget is exhausted by the second 429.
    let error = message.error_message.clone().unwrap_or_default();
    assert!(error.contains("429"), "unexpected error: {error}");
    assert!(
        requests.load(Ordering::SeqCst) >= 2,
        "the scheduled retry re-sent the request"
    );
    let events = events.lock().unwrap().clone();
    assert_eq!(events.len(), 2, "scheduled + settled: {events:?}");
    match events[0] {
        RetryEvent::Scheduled(notice) => {
            assert_eq!(notice.attempt, 1, "upstream auto_retry_start attempt 1");
            assert_eq!(notice.max_attempts, 1);
            assert!(notice.delay_ms > 0, "the countdown delay is positive");
        }
        ref other => panic!("expected Scheduled first, got {other:?}"),
    }
    assert_eq!(events[1], RetryEvent::Settled);
}

#[tokio::test]
async fn session_abort_retry_cancels_the_wait_and_keeps_the_retry_setting() {
    use pir::agent_session::RetryEvent;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    // A long server-requested delay: only the cancellation ends the wait.
    let (base, requests) = retryable_429_endpoint(30_000);
    let agent = retry_session(&base, 2, std::time::Duration::from_secs(60));
    let scheduled = Arc::new(AtomicBool::new(false));
    let events: Arc<Mutex<Vec<RetryEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&events);
    let flag = Arc::clone(&scheduled);
    agent.set_host_retry_notice(Arc::new(move |event| {
        if matches!(event, RetryEvent::Scheduled(_)) {
            flag.store(true, Ordering::SeqCst);
        }
        recorder.lock().unwrap().push(event)
    }));

    let prompt_agent = Arc::clone(&agent);
    let run =
        tokio::spawn(async move { prompt_agent.prompt("hello", Arc::new(|_event| {})).await });
    while !scheduled.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // The TUI's Esc during the countdown (`session.abortRetry()`).
    agent.abort_retry();
    let message = run.await.expect("prompt task").expect("prompt runs");

    let error = message.error_message.clone().unwrap_or_default();
    assert_eq!(
        error, "Retry failed after 1 attempts: Retry cancelled",
        "upstream auto_retry_end finalError text"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the cancelled retry must not re-send the request"
    );
    let events = events.lock().unwrap().clone();
    assert_eq!(
        events.len(),
        2,
        "scheduled + cancelled: expected one dialog pair, got {events:?}"
    );
    assert!(matches!(events[0], RetryEvent::Scheduled(_)), "{events:?}");
    assert!(matches!(events[1], RetryEvent::Cancelled(_)), "{events:?}");
    // `abort_retry` only aborts the pending wait (upstream `abortRetry()`):
    // the auto-retry setting is untouched, so later requests still retry.
    assert!(agent.auto_retry_enabled());
}

#[test]
fn retry_countdown_renders_the_upstream_indicator_text() {
    use pir::tui::terminal::{
        retry_countdown_message, retry_countdown_seconds, retry_status_text, RetryCountdown,
    };
    use std::time::{Duration, Instant};

    // Upstream `RetryStatusIndicator`: `Retrying (a/m) in Ns... (key to cancel)`
    // (status-indicator.ts:55-56 with `keyText("app.interrupt")`).
    assert_eq!(
        retry_status_text(1, 3, 3, "esc"),
        "Retrying (1/3) in 3s... (esc to cancel)"
    );

    // Upstream `CountdownTimer`: `ceil(timeoutMs / 1000)` then one decrement
    // per second.
    let started = Instant::now();
    let delay = Duration::from_millis(3000);
    assert_eq!(retry_countdown_seconds(started, delay, started), 3);
    assert_eq!(
        retry_countdown_seconds(started, delay, started + Duration::from_millis(999)),
        3
    );
    assert_eq!(
        retry_countdown_seconds(started, delay, started + Duration::from_millis(1000)),
        2
    );
    assert_eq!(
        retry_countdown_seconds(started, delay, started + Duration::from_millis(2200)),
        1
    );
    assert_eq!(
        retry_countdown_seconds(started, delay, started + Duration::from_millis(3000)),
        0
    );
    assert_eq!(
        retry_countdown_seconds(started, delay, started + Duration::from_millis(9000)),
        0,
        "the countdown never goes negative"
    );

    let countdown = RetryCountdown {
        attempt: 1,
        max_attempts: 2,
        started,
        delay,
        cancel_key: "esc".to_string(),
    };
    assert_eq!(
        retry_countdown_message(Some(&countdown), started).as_deref(),
        Some("Retrying (1/2) in 3s... (esc to cancel)")
    );
    assert_eq!(retry_countdown_message(None, started), None);
}
