//! Regression: an explicit thinking pick must persist to settings.json.
//!
//! Reported bug: changing the thinking level (and model) was lost after a
//! restart / new session. The TUI's selector-confirm path applied the level
//! to the live session only (`SetThinking`), while the save path wrote
//! settings only (`SetDefaultThinking`) and never applied — so one half of
//! the change was always missing. `persist_default_thinking` is the shared
//! settings half now called by both explicit paths; this test pins that it
//! writes a durable, round-trippable `defaultThinkingLevel`.

use std::sync::Mutex;

use pir::ai::types::ThinkingLevel;
use pir::config::settings::SettingsManager;
use pir::tui::terminal::persist_default_thinking;

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn explicit_thinking_pick_persists_to_settings() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = std::env::temp_dir().join(format!(
        "pir-thinking-persist-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let previous = std::env::var_os("PIR_AGENT_DIR");
    std::env::set_var("PIR_AGENT_DIR", &dir);

    let result = persist_default_thinking("medium");
    assert!(result.is_ok(), "persist failed: {result:?}");

    let cwd = std::env::current_dir().unwrap();
    let reread = SettingsManager::load(&dir, &cwd.join(".pi"), &cwd, false);
    assert_eq!(
        reread.settings.default_thinking_level,
        Some(ThinkingLevel::Medium),
        "an explicit medium pick must survive a settings reload"
    );

    // Raw settings.json round-trip: the persisted value is the wire key, not
    // a display/mapped provider string (the medium→high report).
    let raw = std::fs::read_to_string(dir.join("settings.json")).unwrap_or_default();
    assert!(
        raw.contains("\"defaultThinkingLevel\": \"medium\"")
            || raw.contains("\"defaultThinkingLevel\":\"medium\""),
        "settings.json carries the pi level: {raw}"
    );

    // A second explicit pick replaces the first rather than appending.
    persist_default_thinking("high").expect("second persist");
    let reread = SettingsManager::load(&dir, &cwd.join(".pi"), &cwd, false);
    assert_eq!(
        reread.settings.default_thinking_level,
        Some(ThinkingLevel::High)
    );

    match previous {
        Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
        None => std::env::remove_var("PIR_AGENT_DIR"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
