//! R lane: first-run setup (§2 #18 / §7 #21).
//!
//! Upstream `showFirstTimeSetup` (startup-ui.ts:182-219) asks for a theme and
//! the analytics opt-in exactly once on the first interactive launch, then
//! persists both to the global settings. These tests cover the gate
//! (`shouldRunFirstTimeSetup`), the defaults (theme `system`, analytics
//! preselected to "share"), and the persistence marker: once settings.json
//! exists the prompt never runs again. The invocation gate is checked with
//! explicit booleans, so a non-TTY/print/RPC invocation and a metadata command
//! are covered without touching the process environment.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use pir::config::first_run::{
    apply_first_time_setup, prompt_first_time_setup, should_run_first_time_setup,
    should_run_for_invocation, FirstTimeSetupResult,
};
use pir::config::settings::SettingsManager;

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-first-run-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn load(agent: &Path) -> SettingsManager {
    SettingsManager::load(agent, &agent.join("project"), agent, false)
}

#[test]
fn first_run_setup_asks_once_and_persists_theme_and_analytics() {
    let agent = temp_dir("once");
    let settings_path = agent.join("settings.json");
    assert!(
        should_run_first_time_setup(&settings_path, true, false),
        "first launch (experimental, default agent dir, no settings) asks"
    );

    // Enter/Enter accepts the upstream defaults: system theme + share.
    let result = prompt_first_time_setup(&mut Cursor::new(b"\n\n".to_vec()), &mut Vec::new())
        .expect("prompt")
        .expect("defaults");
    assert_eq!(result.theme, "system");
    assert!(result.share_analytics);

    let mut settings = load(&agent);
    assert!(apply_first_time_setup(&mut settings, &result).is_empty());
    assert_eq!(settings.theme(), Some("system"));
    assert!(settings.enable_analytics());
    let tracking = settings
        .tracking_id()
        .expect("opt-in generates a tracking id")
        .to_string();
    assert_eq!(tracking.len(), 36, "uuid shape: {tracking}");
    assert_eq!(tracking.matches('-').count(), 4);

    // The persisted settings are the "already asked" marker.
    assert!(
        !should_run_first_time_setup(&settings_path, true, false),
        "the second launch must not ask again"
    );
    let persisted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&settings_path).expect("settings.json written"),
    )
    .expect("valid json");
    assert_eq!(persisted["theme"], "system");
    assert_eq!(persisted["enableAnalytics"], serde_json::json!(true));
    assert_eq!(persisted["trackingId"], serde_json::json!(tracking));
    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn first_run_setup_opt_out_keeps_no_tracking_id() {
    let agent = temp_dir("opt-out");
    let result = prompt_first_time_setup(&mut Cursor::new(b"2\n2\n".to_vec()), &mut Vec::new())
        .expect("prompt")
        .expect("explicit choices");
    assert_eq!(result.theme, "dark");
    assert!(!result.share_analytics);

    let mut settings = load(&agent);
    assert!(apply_first_time_setup(&mut settings, &result).is_empty());
    assert_eq!(settings.theme(), Some("dark"));
    assert!(!settings.enable_analytics());
    assert!(
        settings.tracking_id().is_none(),
        "opting out never generates a tracking id"
    );
    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn first_run_prompt_presents_upstream_choices_and_defaults() {
    // Upstream `THEME_OPTIONS` / `ANALYTICS_OPTIONS` order and labels, with
    // Enter accepting the preselected entry (index 0 in both steps).
    let mut writer: Vec<u8> = Vec::new();
    let result = prompt_first_time_setup(&mut Cursor::new(b"\n\n".to_vec()), &mut writer)
        .expect("prompt")
        .expect("defaults");
    let text = String::from_utf8(writer).expect("utf-8 prompt");
    for expected in [
        "Pick a theme.",
        "System (matches your terminal colors)",
        "Dark",
        "Light",
        "Theme [1]:",
        "Opt-in to anonymous usage data sharing?",
        "Share anonymous usage data",
        "Don't share",
        "Analytics [1]:",
    ] {
        assert!(
            text.contains(expected),
            "prompt omitted {expected:?}:\n{text}"
        );
    }
    assert_eq!(result.theme, "system");
    assert!(result.share_analytics);
}

#[test]
fn first_run_setup_cancel_writes_nothing() {
    let agent = temp_dir("cancel");
    let settings_path = agent.join("settings.json");
    // EOF and an out-of-range choice both cancel (upstream Esc → finish(
    // undefined): settings stay absent, so the next launch asks again).
    assert_eq!(
        prompt_first_time_setup(&mut Cursor::new(Vec::new()), &mut Vec::new()).unwrap(),
        None
    );
    assert_eq!(
        prompt_first_time_setup(&mut Cursor::new(b"9\n".to_vec()), &mut Vec::new()).unwrap(),
        None
    );
    assert!(!settings_path.exists());
    assert!(should_run_first_time_setup(&settings_path, true, false));
    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn first_run_setup_is_skipped_outside_the_tty_interactive_path() {
    // Print/JSON/RPC (and any non-TTY invocation) resolve to a non-interactive
    // app mode and pass `interactive = false`.
    assert!(!should_run_for_invocation(false, false));
    // `--help`/`--list-models`/`--export` are metadata commands.
    assert!(!should_run_for_invocation(true, true));
    assert!(should_run_for_invocation(true, false));

    // The settings-file marker alone does not bypass the other gates.
    let agent = temp_dir("gates");
    let settings_path = agent.join("settings.json");
    assert!(should_run_first_time_setup(&settings_path, true, false));
    assert!(
        !should_run_first_time_setup(&settings_path, false, false),
        "experimental features are required (upstream PI_EXPERIMENTAL)"
    );
    assert!(
        !should_run_first_time_setup(&settings_path, true, true),
        "a custom agent directory skips the first-run flow"
    );
    std::fs::write(&settings_path, "{}").expect("write settings");
    assert!(
        !should_run_first_time_setup(&settings_path, true, false),
        "an existing settings.json means setup already ran"
    );
    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn analytics_opt_in_keeps_an_existing_tracking_id_across_toggles() {
    let agent = temp_dir("tracking");
    let mut settings = load(&agent);
    settings.set_enable_analytics(true);
    let first = settings.tracking_id().expect("generated").to_string();
    settings.set_enable_analytics(false);
    assert!(!settings.enable_analytics());
    assert_eq!(settings.tracking_id(), Some(first.as_str()));
    settings.set_enable_analytics(true);
    assert_eq!(
        settings.tracking_id(),
        Some(first.as_str()),
        "re-opting in reuses the identifier"
    );
    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn apply_first_time_setup_accepts_an_explicit_choice_struct() {
    // The result type is the wiring contract between the prompt and the
    // settings write (upstream `FirstTimeSetupResult`).
    let agent = temp_dir("explicit");
    let mut settings = load(&agent);
    let result = FirstTimeSetupResult {
        theme: "light".to_string(),
        share_analytics: false,
    };
    assert!(apply_first_time_setup(&mut settings, &result).is_empty());
    assert_eq!(settings.theme(), Some("light"));
    let _ = std::fs::remove_dir_all(&agent);
}
