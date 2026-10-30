//! Regression: a thinking-level change must refresh the two footer rows
//! (model • level) immediately.
//!
//! Reported bug: after selecting a thinking level through `/thinking` or the
//! thinking-cycle shortcut, the status rows below the editor kept the old
//! level/model state until some unrelated event refreshed them.
//!
//! The host funnel is [`Footer::update_thinking_level`], called by
//! `App::apply_local_thinking` (upstream `selectThinkingLevel` /
//! `cycleThinkingLevel` → `footer.invalidate()`). This test changes the level
//! through that exact seam and asserts the rendered rows move on the next
//! render — without any other footer input changing or an intervening event.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use pir::ai::types::{Model, ModelCost, ModelCostRates, ThinkingLevel};
use pir::tui::app::{footer_row_rebuilds, reset_footer_row_rebuilds, Footer, FooterState};
use pir::tui::terminal::active_thinking_levels;
use pir::tui::theme::set_theme;
use pir::tui::Component;

/// The palette is process-global; serialize this binary's tests.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn footer_state() -> FooterState {
    FooterState {
        cwd: "/work/project".into(),
        git_branch: Some("main".into()),
        model_id: "gpt-5".into(),
        model_provider: "openai".into(),
        model_reasoning: true,
        thinking_level: "low".into(),
        ..FooterState::default()
    }
}

fn strip(lines: Vec<String>) -> String {
    lines
        .into_iter()
        .map(|line| pir::tui::ansi::strip_ansi(&line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn thinking_level_change_refreshes_the_footer_immediately() {
    let _guard = lock();
    set_theme("dark");

    let footer = Footer::new(footer_state());
    let before = strip(footer.render(120));
    assert!(before.contains("gpt-5 • low"), "before: {before}");

    assert!(
        footer.update_thinking_level("high"),
        "a distinct level must report a change"
    );

    // NO other event/refresh: the very next render must show the new level.
    let after = strip(footer.render(120));
    assert!(after.contains("gpt-5 • high"), "after: {after}");
    assert!(!after.contains("gpt-5 • low"), "stale level: {after}");

    // The memo was invalidated (one rebuild on the changed frame), and a
    // same-level call is a no-op that keeps it warm.
    reset_footer_row_rebuilds();
    let _ = footer.render(120);
    assert_eq!(footer_row_rebuilds(), 0, "steady frame stays memoized");
    assert!(
        !footer.update_thinking_level("high"),
        "same-level update reports no change"
    );
    let _ = footer.render(120);
    assert_eq!(
        footer_row_rebuilds(),
        0,
        "same-level update keeps memo warm"
    );
}

/// The render must also move when the level is the ONLY changed input in a
/// full `FooterState` replacement (the `refresh_footer` path a model change
/// uses): `FooterState` equality includes the level.
#[test]
fn footer_state_replacement_with_only_a_thinking_change_rebuilds() {
    let _guard = lock();
    set_theme("dark");

    let footer = Footer::new(footer_state());
    let _ = footer.render(100);
    let mut next = footer_state();
    next.thinking_level = "max".into();
    footer.update(next);
    reset_footer_row_rebuilds();
    let rows = strip(footer.render(100));
    assert!(rows.contains("gpt-5 • max"), "rows: {rows}");
    assert_eq!(footer_row_rebuilds(), 1);
}

#[test]
fn non_reasoning_model_footer_still_ignores_the_level() {
    let _guard = lock();
    set_theme("dark");

    let mut state = footer_state();
    state.model_reasoning = false;
    let footer = Footer::new(state);
    footer.update_thinking_level("high");
    let rows = strip(footer.render(120));
    assert!(rows.contains("gpt-5"), "rows: {rows}");
    assert!(
        !rows.contains("• high"),
        "a non-reasoning model never renders a level: {rows}"
    );
}

fn reasoning_model(map: Option<BTreeMap<String, Option<String>>>) -> Model {
    Model {
        id: "m".into(),
        name: "m".into(),
        api: "openai-completions".into(),
        provider: "p".into(),
        base_url: "https://example.invalid/v1".into(),
        reasoning: true,
        thinking_level_map: map,
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
        context_window: 32_000,
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

/// The reported "selected medium, log shows high": the selector used a static
/// seven-level ladder, but this model's `thinkingLevelMap` marks `medium` as
/// unsupported (`null`). `setThinkingLevel` then clamped the pick to `high`.
/// The selector/`/thinking` must offer exactly the model-supported ladder, so
/// `medium` is never selectable there.
#[test]
fn thinking_selector_ladder_excludes_model_unsupported_levels() {
    let mut map = BTreeMap::new();
    map.insert("medium".to_string(), None);
    map.insert("high".to_string(), Some("high".to_string()));
    let model = reasoning_model(Some(map));

    let levels = active_thinking_levels(Some(&model));
    assert_eq!(levels, vec!["off", "minimal", "low", "high"], "{levels:?}");
    assert!(!levels.iter().any(|level| level == "medium"));
    assert!(levels.iter().any(|level| level == "high"));
    assert!(!pir::ai::types::thinking_level_supported(
        &model,
        ThinkingLevel::Medium
    ));

    // A model without a map supports the standard ladder (xhigh/max absent →
    // unsupported, matching upstream).
    let plain = reasoning_model(None);
    assert_eq!(
        active_thinking_levels(Some(&plain)),
        vec!["off", "minimal", "low", "medium", "high"]
    );

    // No configured model: the selector still opens on the full ladder.
    assert_eq!(active_thinking_levels(None).len(), 7);
}
