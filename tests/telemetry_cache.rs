//! Tests for the process-level install-telemetry settings cache in
//! `pir::ai::provider::install_telemetry_enabled` (provider.rs).
//!
//! The settings-derived value is computed once per process and cached; the
//! `PI_TELEMETRY` env override must stay live and win over a cached value in
//! either direction. The test points `PIR_AGENT_DIR` at a scratch dir holding
//! a `settings.json` with `enableInstallTelemetry: false`, so the cached
//! (settings) value is "disabled" and the env override's win is observable.

use std::path::PathBuf;
use std::sync::Mutex;

/// Serializes every test here: they all mutate the same process env and share
/// the same process-global telemetry cache.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Scratch agent dir holding a `settings.json` that disables install
/// telemetry.
fn disabled_settings_agent_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("pir-telemetry-cache-tests")
        .join(format!("{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch agent dir");
    std::fs::write(
        dir.join("settings.json"),
        r#"{"enableInstallTelemetry": false}"#,
    )
    .expect("write settings.json");
    dir
}

fn set_env(key: &str, value: Option<&str>) {
    // Edition 2021: std::env::set_var/remove_var are safe.
    match value {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
    }
}

#[test]
fn env_override_wins_over_cached_disabled_value() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = disabled_settings_agent_dir("env-override");
    set_env("PIR_AGENT_DIR", Some(dir.to_str().expect("utf8 path")));

    // First call populates the cache from settings: disabled.
    set_env("PI_TELEMETRY", None);
    assert!(
        !pir::ai::provider::install_telemetry_enabled(),
        "settings disable install telemetry"
    );

    // Env override wins in both directions even though the cache holds
    // "disabled", and never touches the cache.
    set_env("PI_TELEMETRY", Some("1"));
    assert!(pir::ai::provider::install_telemetry_enabled());
    set_env("PI_TELEMETRY", Some("0"));
    assert!(!pir::ai::provider::install_telemetry_enabled());

    // Back to no override: the cached settings value is served again.
    set_env("PI_TELEMETRY", None);
    assert!(!pir::ai::provider::install_telemetry_enabled());

    set_env("PIR_AGENT_DIR", None);
}

#[test]
fn cached_value_is_stable_across_calls() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = disabled_settings_agent_dir("stable");
    set_env("PIR_AGENT_DIR", Some(dir.to_str().expect("utf8 path")));
    set_env("PI_TELEMETRY", None);

    // Whichever call computes the cache first, every later call must serve
    // the identical cached value without re-reading settings.
    let first = pir::ai::provider::install_telemetry_enabled();
    for _ in 0..3 {
        assert_eq!(pir::ai::provider::install_telemetry_enabled(), first);
    }

    set_env("PIR_AGENT_DIR", None);
}
