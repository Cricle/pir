//! Config-settings unit tests moved out of `src/config/settings/**` inline
//! `#[cfg(test)]` modules:
//!
//! - `src/config/settings/env.rs` → `shell_prefs_from` profile defaults
//!   (`performance_shell_prefs_tests`);
//! - `src/config/settings/schema.rs` → `PerformanceSettings` bounded defaults
//!   and override precedence (`performance_settings_tests`).
//!
//! Both used `use super::*` inside the module; the assertions only touch
//! public `pir::config::settings` items, so they move unchanged apart from the
//! import path.

use pir::config::settings::{
    shell_prefs_from, PerformanceProfile, PerformanceSettings, PromptCacheMode, Settings,
    ToolsSettings,
};

// ---------------------------------------------------------------------------
// src/config/settings/env.rs — performance_shell_prefs_tests
// ---------------------------------------------------------------------------

#[test]
fn efficient_profile_uses_a_short_default_command_timeout() {
    let settings = Settings {
        performance: Some(PerformanceSettings {
            profile: Some(PerformanceProfile::Efficient),
            ..Default::default()
        }),
        ..Settings::default()
    };
    let prefs = shell_prefs_from(&settings, None);
    assert_eq!(prefs.effective_command_timeout_ms(), 30_000);
    assert!(prefs.restrict_broad_search);
}

#[test]
fn balanced_profile_keeps_broad_search_defense_on_by_default() {
    let prefs = shell_prefs_from(&Settings::default(), None);
    assert_eq!(prefs.effective_command_timeout_ms(), 120_000);
    assert!(prefs.restrict_broad_search);
}

#[test]
fn tools_broad_search_guard_false_disables_the_defense() {
    let mut settings = Settings::default();
    settings.tools = Some(ToolsSettings {
        command_timeout_ms: None,
        broad_search_guard: Some(false),
    });
    let prefs = shell_prefs_from(&settings, None);
    assert!(!prefs.restrict_broad_search);
}

// ---------------------------------------------------------------------------
// src/config/settings/schema.rs — performance_settings_tests
// ---------------------------------------------------------------------------

#[test]
fn balanced_profile_keeps_historical_defaults() {
    let settings = PerformanceSettings::default();
    assert_eq!(settings.profile(), PerformanceProfile::Balanced);
    assert_eq!(settings.effective_tool_result_max_bytes(), 0);
    assert_eq!(settings.effective_soft_turn_budget(), 0);
    assert_eq!(settings.effective_hard_turn_budget(), 0);
    assert_eq!(settings.effective_max_total_tokens(), 0);
    assert_eq!(settings.effective_context_files_max_bytes(), 0);
    assert!(settings.prompt_cache_enabled());
}

#[test]
fn efficient_profile_has_bounded_defaults_and_overrides_win() {
    let settings = PerformanceSettings {
        profile: Some(PerformanceProfile::Efficient),
        ..Default::default()
    };
    assert_eq!(settings.effective_tool_result_max_bytes(), 32 * 1024);
    assert_eq!(settings.effective_soft_turn_budget(), 24);
    assert_eq!(settings.effective_hard_turn_budget(), 48);
    assert_eq!(settings.effective_max_total_tokens(), 250_000);
    assert_eq!(settings.effective_context_files_max_bytes(), 64 * 1024);

    let overridden = PerformanceSettings {
        profile: Some(PerformanceProfile::Efficient),
        tool_result_max_bytes: Some(1),
        soft_turn_budget: Some(2),
        hard_turn_budget: Some(3),
        max_total_tokens: Some(4),
        context_files_max_bytes: Some(5),
        prompt_cache: Some(PromptCacheMode::Off),
    };
    assert_eq!(overridden.effective_tool_result_max_bytes(), 1);
    assert_eq!(overridden.effective_soft_turn_budget(), 2);
    assert_eq!(overridden.effective_hard_turn_budget(), 3);
    assert_eq!(overridden.effective_max_total_tokens(), 4);
    assert_eq!(overridden.effective_context_files_max_bytes(), 5);
    assert!(!overridden.prompt_cache_enabled());
}
