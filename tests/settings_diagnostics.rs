//! Startup settings diagnostics parity (PLAN.md §6c item 5).
//!
//! Upstream `core/settings-diagnostics.ts` turns recorded settings load
//! failures into warning diagnostics (`Invalid settings file <path>: <error>`
//! when the file is known, else `Invalid <scope> settings: <error>`) and
//! dedupes them by `(type, message)`. These tests cover the upstream-shaped
//! collection, the manager's NotFound-vs-read-error split, the pir-only
//! per-field warnings staying out of the list, and the CLI's stderr report.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use pir::config::settings::{
    collect_settings_diagnostics, deduplicate_diagnostics, PackageSource, SettingsDiagnostic,
    SettingsDiagnosticType, SettingsError, SettingsManager, SettingsScope, TerminalImagesMode,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A per-test temp directory. Unique even across tests in the same process.
fn temp_dir(label: &str) -> std::path::PathBuf {
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pir-settings-diagnostics-{label}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(path, text).expect("write settings file");
}

#[test]
fn malformed_file_uses_exact_path_prefix() {
    let dir = temp_dir("malformed");
    let settings_path = dir.join("settings.json");
    write_file(&settings_path, "{ this is not json");

    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    let diagnostics = collect_settings_diagnostics(&mut manager);

    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0].type_, SettingsDiagnosticType::Warning);
    let expected_prefix = format!("Invalid settings file {}: ", settings_path.display());
    assert!(
        diagnostics[0].message.starts_with(&expected_prefix),
        "message {:?} does not start with {expected_prefix:?}",
        diagnostics[0].message
    );
    // The non-interactive report prefixes warnings exactly as upstream
    // `reportDiagnostics` does.
    assert!(diagnostics[0]
        .stderr_line()
        .starts_with("Warning: Invalid settings file "));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn error_without_path_falls_back_to_scope_message() {
    let project = SettingsError {
        scope: SettingsScope::Project,
        path: None,
        message: "boom".to_string(),
    };
    assert_eq!(
        project.diagnostic_message(),
        "Invalid project settings: boom"
    );

    let global = SettingsError {
        scope: SettingsScope::Global,
        path: None,
        message: "bad".to_string(),
    };
    assert_eq!(global.diagnostic_message(), "Invalid global settings: bad");

    // With a path the file form wins over the scope form.
    let with_path = SettingsError {
        scope: SettingsScope::Global,
        path: Some(std::path::PathBuf::from("/tmp/settings.json")),
        message: "bad".to_string(),
    };
    assert_eq!(
        with_path.diagnostic_message(),
        "Invalid settings file /tmp/settings.json: bad"
    );
}

#[test]
fn dedupe_collapses_warnings_but_keeps_distinct_types() {
    let warning = SettingsDiagnostic {
        type_: SettingsDiagnosticType::Warning,
        message: "same message".to_string(),
    };
    let diagnostics = vec![
        warning.clone(),
        warning.clone(),
        SettingsDiagnostic {
            type_: SettingsDiagnosticType::Error,
            message: "same message".to_string(),
        },
        SettingsDiagnostic {
            type_: SettingsDiagnosticType::Warning,
            message: "other message".to_string(),
        },
    ];

    let deduped = deduplicate_diagnostics(diagnostics);
    assert_eq!(deduped.len(), 3, "{deduped:?}");
    // First wins, and only the duplicate (type, message) pair is removed.
    assert_eq!(deduped[0], warning);
    assert_eq!(deduped[1].type_, SettingsDiagnosticType::Error);
    assert_eq!(deduped[2].message, "other message");
}

#[test]
fn collect_drains_errors_one_shot() {
    let dir = temp_dir("drain");
    write_file(&dir.join("settings.json"), "{ nope");

    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    assert_eq!(collect_settings_diagnostics(&mut manager).len(), 1);
    assert!(
        collect_settings_diagnostics(&mut manager).is_empty(),
        "a drained error must not be reported twice"
    );
    assert!(manager.drain_errors().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn non_object_root_is_reported_with_path() {
    let dir = temp_dir("non-object");
    let settings_path = dir.join("settings.json");
    write_file(&settings_path, "[]");

    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    let diagnostics = collect_settings_diagnostics(&mut manager);

    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let expected = format!(
        "Invalid settings file {}: settings must be a JSON object",
        settings_path.display()
    );
    assert_eq!(diagnostics[0].message, expected);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn per_field_warnings_stay_out_of_upstream_shaped_diagnostics() {
    let dir = temp_dir("field-warning");
    // Valid JSON object; one wrong-typed field falls back to its default via
    // pir's tolerant per-field parse. That is a pir-only superset and must
    // not appear as an upstream load diagnostic.
    write_file(&dir.join("settings.json"), r#"{"theme": 123}"#);

    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    assert!(
        !manager.warning_messages().is_empty(),
        "expected a per-field warning"
    );
    assert!(
        collect_settings_diagnostics(&mut manager).is_empty(),
        "per-field warnings must not become upstream load diagnostics"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Leaf-level tolerant recovery (audit #4)
// ---------------------------------------------------------------------------

#[test]
fn invalid_nested_leaf_keeps_valid_siblings() {
    let dir = temp_dir("leaf-recovery");
    // `trueColor` is a string that is not "auto" (wrong type); `images` is a
    // valid sibling and must survive instead of being lost with the block.
    write_file(
        &dir.join("settings.json"),
        r#"{"terminal": {"trueColor": "yes", "images": "kitty"}}"#,
    );

    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    let terminal = manager
        .settings
        .terminal
        .as_ref()
        .expect("the terminal block must survive a bad leaf");
    assert_eq!(terminal.images, Some(TerminalImagesMode::Kitty));
    assert_eq!(terminal.true_color, None);

    let warnings = manager.warning_messages();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].starts_with("global settings: invalid setting \"terminal.trueColor\""),
        "unexpected warning shape: {warnings:?}"
    );
    assert!(
        collect_settings_diagnostics(&mut manager).is_empty(),
        "leaf warnings must stay out of upstream load diagnostics"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mixed_valid_and_invalid_siblings_recover_independently() {
    let dir = temp_dir("mixed-siblings");
    write_file(
        &dir.join("settings.json"),
        r#"{
            "terminal": {
                "trueColor": 1,
                "images": "iterm2",
                "showImages": "lots",
                "clearOnShrink": true
            },
            "images": { "autoResize": false, "blockImages": "no" },
            "theme": "dark"
        }"#,
    );

    let manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    let terminal = manager
        .settings
        .terminal
        .as_ref()
        .expect("terminal block survives");
    assert_eq!(terminal.images, Some(TerminalImagesMode::Iterm2));
    assert_eq!(terminal.true_color, None);
    assert_eq!(terminal.show_images, None);
    assert_eq!(terminal.clear_on_shrink, Some(true));

    let images = manager
        .settings
        .images
        .as_ref()
        .expect("images block survives");
    assert_eq!(images.auto_resize, Some(false));
    assert_eq!(images.block_images, None);
    assert_eq!(manager.settings.theme.as_deref(), Some("dark"));

    let warnings = manager.warning_messages();
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    for path in [
        "\"terminal.trueColor\"",
        "\"terminal.showImages\"",
        "\"images.blockImages\"",
    ] {
        assert!(
            warnings.iter().any(|warning| warning.contains(path)),
            "missing warning for {path}: {warnings:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn deep_nested_leaf_recovery_keeps_sibling_leaves() {
    let dir = temp_dir("deep-leaf");
    write_file(
        &dir.join("settings.json"),
        r#"{
            "retry": {
                "maxRetries": 5,
                "provider": { "timeoutMs": 1000, "maxRetries": "many" }
            },
            "compaction": {
                "enabled": false,
                "reserveTokens": "big",
                "modelOverrides": {
                    "p/m": { "reserveTokens": 7, "keepRecentTokens": -1 }
                }
            }
        }"#,
    );

    let manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    let retry = manager.settings.retry.as_ref().expect("retry survives");
    assert_eq!(retry.max_retries, Some(5));
    let provider = retry.provider.as_ref().expect("provider survives");
    assert_eq!(provider.timeout_ms, Some(1000));
    assert_eq!(provider.max_retries, None);

    let compaction = manager
        .settings
        .compaction
        .as_ref()
        .expect("compaction survives");
    assert_eq!(compaction.enabled, Some(false));
    assert_eq!(compaction.reserve_tokens, None);
    let overrides = compaction
        .model_overrides
        .as_ref()
        .expect("model overrides survive");
    assert_eq!(overrides["p/m"].reserve_tokens, Some(7));
    assert_eq!(overrides["p/m"].keep_recent_tokens, None);

    let warnings = manager.warning_messages();
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    for path in [
        "\"retry.provider.maxRetries\"",
        "\"compaction.reserveTokens\"",
        "\"compaction.modelOverrides.p/m.keepRecentTokens\"",
    ] {
        assert!(
            warnings.iter().any(|warning| warning.contains(path)),
            "missing warning for {path}: {warnings:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_fields_are_still_ignored_during_recovery() {
    let dir = temp_dir("unknown-during-recovery");
    // The bad `trueColor` forces the recovery path; the unknown top-level and
    // nested keys must still be accepted (top-level ones land in `extra`).
    write_file(
        &dir.join("settings.json"),
        r#"{
            "terminal": {
                "trueColor": "sometimes",
                "images": "kitty",
                "mysteryKnob": { "depth": 1 }
            },
            "extensionSetting": { "enabled": true }
        }"#,
    );

    let manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    let terminal = manager
        .settings
        .terminal
        .as_ref()
        .expect("terminal survives");
    assert_eq!(terminal.images, Some(TerminalImagesMode::Kitty));
    assert_eq!(terminal.true_color, None);
    assert_eq!(
        manager.settings.extra.get("extensionSetting"),
        Some(&serde_json::json!({ "enabled": true }))
    );
    let warnings = manager.warning_messages();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("\"terminal.trueColor\""),
        "{warnings:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn top_level_type_error_still_falls_back_for_the_whole_key() {
    let dir = temp_dir("top-level-type");
    write_file(
        &dir.join("settings.json"),
        r#"{"terminal": 42, "theme": "dark"}"#,
    );

    let manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    assert!(manager.settings.terminal.is_none());
    assert_eq!(manager.settings.theme.as_deref(), Some("dark"));

    let warnings = manager.warning_messages();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].starts_with("global settings: invalid setting \"terminal\""),
        "unexpected warning shape: {warnings:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_array_element_is_dropped_without_losing_valid_elements() {
    let dir = temp_dir("array-elements");
    write_file(
        &dir.join("settings.json"),
        r#"{
            "packages": ["npm:left-pad", 42, "npm:right-pad"],
            "defaultTools": ["read", false, "bash"]
        }"#,
    );

    let manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    assert_eq!(
        manager.settings.packages,
        Some(vec![
            PackageSource::Source("npm:left-pad".to_string()),
            PackageSource::Source("npm:right-pad".to_string()),
        ])
    );
    assert_eq!(
        manager.settings.default_tools,
        Some(vec!["read".to_string(), "bash".to_string()])
    );

    let warnings = manager.warning_messages();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("\"packages[1]\"")),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("\"defaultTools[1]\"")),
        "{warnings:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nested_warnings_are_attributed_to_the_scope_owning_the_bad_leaf() {
    let global = temp_dir("leaf-scope-global");
    let project = temp_dir("leaf-scope-project");
    write_file(
        &global.join("settings.json"),
        r#"{"terminal": {"trueColor": "bad", "clearOnShrink": true}}"#,
    );
    write_file(
        &project.join("settings.json"),
        r#"{"terminal": {"images": "kitty", "showImages": "bad"}}"#,
    );

    let manager = SettingsManager::load(&global, &project, &project, true);
    let terminal = manager
        .settings
        .terminal
        .as_ref()
        .expect("merged terminal survives");
    assert_eq!(terminal.images, Some(TerminalImagesMode::Kitty));
    assert_eq!(terminal.clear_on_shrink, Some(true));
    assert_eq!(terminal.true_color, None);
    assert_eq!(terminal.show_images, None);

    let warnings = manager.warning_messages();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings.iter().any(|warning| warning
            .starts_with("global settings: invalid setting \"terminal.trueColor\"")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|warning| warning
            .starts_with("project settings: invalid setting \"terminal.showImages\"")),
        "{warnings:?}"
    );

    let _ = std::fs::remove_dir_all(&global);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn valid_nested_config_records_no_warnings() {
    let dir = temp_dir("valid-nested");
    write_file(
        &dir.join("settings.json"),
        r#"{
            "terminal": { "trueColor": "auto", "images": "kitty" },
            "retry": { "provider": { "timeoutMs": 5 } }
        }"#,
    );

    let manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    assert!(
        manager.warning_messages().is_empty(),
        "valid settings must not warn: {:?}",
        manager.warning_messages()
    );
    assert_eq!(
        manager
            .settings
            .terminal
            .as_ref()
            .and_then(|terminal| terminal.images),
        Some(TerminalImagesMode::Kitty)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn unreadable_file_is_diagnosed_with_path() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("unreadable");
    let settings_path = dir.join("settings.json");
    write_file(&settings_path, "{}");
    std::fs::set_permissions(&settings_path, std::fs::Permissions::from_mode(0o000))
        .expect("chmod settings.json");

    // Running as root ignores file modes: skip rather than report a false
    // failure (the read-error path is still exercised by the non-object test
    // on every platform).
    if std::fs::read_to_string(&settings_path).is_ok() {
        let _ = std::fs::set_permissions(&settings_path, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    let diagnostics = collect_settings_diagnostics(&mut manager);

    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let expected_prefix = format!("Invalid settings file {}: ", settings_path.display());
    assert!(
        diagnostics[0].message.starts_with(&expected_prefix),
        "message {:?} does not start with {expected_prefix:?}",
        diagnostics[0].message
    );

    let _ = std::fs::set_permissions(&settings_path, std::fs::Permissions::from_mode(0o600));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn list_models_cli_reports_invalid_settings_on_stderr() {
    let agent_dir = temp_dir("cli-agent");
    let project_dir = temp_dir("cli-project");
    let settings_path = agent_dir.join("settings.json");
    write_file(&settings_path, "{ definitely not json");

    let output = Command::new(env!("CARGO_BIN_EXE_pir"))
        .arg("--list-models")
        .env("PIR_AGENT_DIR", &agent_dir)
        .env("PIR_OFFLINE", "1")
        .current_dir(&project_dir)
        .output()
        .expect("pir binary should start");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "Warning: Invalid settings file {}",
            settings_path.display()
        )),
        "stderr did not report the invalid settings file: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::remove_dir_all(&project_dir);
}
