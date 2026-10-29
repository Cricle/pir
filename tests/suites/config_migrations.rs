//! Unit tests for the startup migrations (`src/config/migrations.rs`,
//! upstream `migrations.ts`) and the `PI_STARTUP_BENCHMARK` timing helper
//! (`src/config/timings.rs`, upstream `core/timings.ts`).
//!
//! The suites exercise [`pir::config::migrations::run_migrations_in`] with
//! explicit temp directories: mutating `PIR_AGENT_DIR` would leak across the
//! parallel test binaries (the environment is process-wide), so nothing here
//! touches the environment.

use std::fs;
use std::path::{Path, PathBuf};

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-migrations-test-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir");
    dir
}

// ---------------------------------------------------------------------------
// auth → auth.json
// ---------------------------------------------------------------------------

#[test]
fn auth_migrates_oauth_and_apikeys_and_is_idempotent() {
    let agent = temp_dir("auth");
    fs::write(
        agent.join("oauth.json"),
        r#"{ "anthropic": { "access": "a", "refresh": "r", "expires": 1 } }"#,
    )
    .expect("write oauth");
    fs::write(
        agent.join("settings.json"),
        r#"{ "theme": "dark", "apiKeys": { "openrouter": "sk-or", "anthropic": "ignored" } }"#,
    )
    .expect("write settings");

    let result = pir::config::migrations::run_migrations_in(&agent, Path::new("."));
    // OAuth providers migrate first; the settings api-key for an already
    // migrated provider is skipped (upstream `if (!migrated[provider] ...)`).
    assert_eq!(result.migrated_auth_providers, ["anthropic", "openrouter"]);
    let auth: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(agent.join("auth.json")).expect("read auth"))
            .expect("json");
    assert_eq!(auth["anthropic"]["type"], "oauth");
    assert_eq!(auth["anthropic"]["access"], "a");
    assert_eq!(auth["openrouter"]["type"], "api_key");
    assert_eq!(auth["openrouter"]["key"], "sk-or");
    assert!(agent.join("oauth.json.migrated").exists());
    assert!(!agent.join("oauth.json").exists());
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(agent.join("settings.json")).expect("read"))
            .expect("json");
    assert_eq!(settings["theme"], "dark");
    assert!(settings.get("apiKeys").is_none());

    // Second run: auth.json exists → no migration, no providers.
    let result = pir::config::migrations::run_migrations_in(&agent, Path::new("."));
    assert!(result.migrated_auth_providers.is_empty());
    let _ = fs::remove_dir_all(&agent);
}

// ---------------------------------------------------------------------------
// stray agent-root sessions → per-cwd dirs
// ---------------------------------------------------------------------------

#[test]
fn agent_root_sessions_move_into_cwd_dirs_and_are_idempotent() {
    let agent = temp_dir("sessions");
    let cwd = temp_dir("sessions-cwd");
    // Build the header with serde_json: the raw Windows cwd path contains
    // backslashes that must be JSON-escaped (a string-replace would emit
    // invalid `\U`-style escapes and the header would not parse).
    let header = serde_json::json!({
        "type": "session",
        "id": "abcd1234",
        "cwd": cwd.to_string_lossy(),
    });
    fs::write(
        agent.join("stray.jsonl"),
        header.to_string() + "\n{\"type\":\"message\"}\n",
    )
    .expect("write session");
    // A file without a session header stays put (old pir v4 files are
    // skipped, not migrated — MAPPING.md).
    fs::write(agent.join("legacy.jsonl"), "{\"type\":\"sessionv4\"}\n").expect("write legacy");

    pir::config::migrations::run_migrations_in(&agent, Path::new("."));

    // The migration lands the file where pir's session lookup finds it: the
    // session-manager encoding of the header cwd, resolved through the
    // explicit-parameter core (`None` home / `.` base — the temp path is
    // absolute, so neither is consulted and the expectation stays hermetic).
    let target = agent
        .join("sessions")
        .join(pir::session_manager::encode_cwd_dir_name(
            &pir::session_manager::resolve_path_str_with(
                &cwd.to_string_lossy(),
                None,
                Path::new("."),
            ),
        ));
    assert!(target.join("stray.jsonl").exists(), "moved to {target:?}");
    assert!(!agent.join("stray.jsonl").exists());
    assert!(
        agent.join("legacy.jsonl").exists(),
        "non-upstream headers stay"
    );

    // Second run: nothing left to move.
    pir::config::migrations::run_migrations_in(&agent, Path::new("."));
    assert!(target.join("stray.jsonl").exists());
    let _ = fs::remove_dir_all(&agent);
    let _ = fs::remove_dir_all(&cwd);
}

// ---------------------------------------------------------------------------
// commands → prompts + deprecated dirs + tools → bin
// ---------------------------------------------------------------------------

#[test]
fn commands_migrate_to_prompts_once_and_deprecated_dirs_warn() {
    let agent = temp_dir("ext");
    let project = temp_dir("ext-project");
    fs::create_dir_all(agent.join("commands")).expect("mkdir");
    fs::write(agent.join("commands").join("review.md"), "do review").expect("write");
    fs::create_dir_all(project.join(".pi").join("commands")).expect("mkdir");
    fs::create_dir_all(project.join(".pi").join("hooks")).expect("mkdir");
    fs::create_dir_all(project.join(".pi").join("tools")).expect("mkdir");
    fs::write(project.join(".pi").join("tools").join("mytool.js"), "x").expect("write");

    let result = pir::config::migrations::run_migrations_in(&agent, &project);
    // commands/ renamed for both scopes; contents intact.
    assert!(agent.join("prompts").join("review.md").exists());
    assert!(!agent.join("commands").exists());
    assert!(project.join(".pi").join("prompts").is_dir());
    assert!(!project.join(".pi").join("commands").exists());
    // Deprecated-dir warnings carry the upstream messages.
    assert_eq!(
        result.deprecation_warnings,
        vec![
            "Project hooks/ directory found. Hooks have been renamed to extensions.".to_string(),
            "Project tools/ directory contains custom tools. Custom tools have been merged into extensions."
                .to_string(),
        ]
    );

    // Idempotent: prompts/ now exists, so nothing renames or re-warns
    // (hooks/ still warns — the directories are not removed upstream either).
    let result = pir::config::migrations::run_migrations_in(&agent, &project);
    assert_eq!(result.deprecation_warnings.len(), 2);
    assert!(agent.join("prompts").join("review.md").exists());
    let _ = fs::remove_dir_all(&agent);
    let _ = fs::remove_dir_all(&project);
}

#[test]
fn managed_binaries_move_tools_to_bin_and_autoextracted_tools_do_not_warn() {
    let agent = temp_dir("bin");
    fs::create_dir_all(agent.join("tools")).expect("mkdir");
    fs::write(agent.join("tools").join("rg.exe"), "binary").expect("write");
    fs::write(agent.join("tools").join("fd"), "binary").expect("write");
    // Auto-extracted binaries alone do not count as custom tools.
    let project = temp_dir("bin-project");
    let warnings =
        pir::config::migrations::run_migrations_in(&agent, &project).deprecation_warnings;
    assert!(warnings.is_empty());

    assert!(pir::config::migrations::bin_dir(&agent)
        .join("rg.exe")
        .exists());
    assert!(pir::config::migrations::bin_dir(&agent).join("fd").exists());
    assert!(!agent.join("tools").join("rg.exe").exists());
    assert!(!agent.join("tools").join("fd").exists());

    // Idempotent: the sources are gone, the second run moves nothing.
    pir::config::migrations::run_migrations_in(&agent, Path::new("."));
    assert!(pir::config::migrations::bin_dir(&agent)
        .join("rg.exe")
        .exists());
    let _ = fs::remove_dir_all(&agent);
    let _ = fs::remove_dir_all(&project);
}

// ---------------------------------------------------------------------------
// PI_STARTUP_BENCHMARK timings
// ---------------------------------------------------------------------------

#[test]
fn startup_benchmark_flag_parses_upstream_truthy_values() {
    assert!(pir::config::timings::is_truthy_env_flag(Some("1")));
    assert!(pir::config::timings::is_truthy_env_flag(Some("true")));
    assert!(pir::config::timings::is_truthy_env_flag(Some("YES")));
    assert!(!pir::config::timings::is_truthy_env_flag(Some("0")));
    assert!(!pir::config::timings::is_truthy_env_flag(Some("")));
    assert!(!pir::config::timings::is_truthy_env_flag(None));
    assert!(!pir::config::timings::is_truthy_env_flag(Some("off")));
}

#[test]
fn startup_timings_record_phases_and_stay_silent_when_disabled() {
    // Disabled: `time` records nothing and printing is a no-op.
    let mut timings = pir::config::timings::StartupTimings::with_enabled(false);
    timings.time("phase");
    assert!(!timings.enabled());
    assert!(timings.entries().is_empty());

    // Enabled: the phase list accumulates in call order.
    let mut enabled = pir::config::timings::StartupTimings::with_enabled(true);
    enabled.time("runMigrations");
    enabled.time("loadSettings");
    let entries = enabled.entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, "runMigrations");
    assert_eq!(entries[1].0, "loadSettings");
    // Printing writes the upstream report shape to stderr (visible only when
    // this test runs with output shown).
    enabled.print_to_stderr();
}
