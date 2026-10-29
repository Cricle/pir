//! Unit tests for the config layer migrated out of `src/config/*` inline
//! `#[cfg(test)]` modules: env flags, keybindings parsing/migration, MCP
//! config parsing/validation, settings loading/merging/persistence, and the
//! project-trust store/resolution.

use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// src/config/env.rs
// ---------------------------------------------------------------------------

use pir::config::env::env_flag;

#[test]
fn telemetry_override_parses_known_values() {
    // Only the parse logic is exercised here; the process environment is
    // not mutated (tests run in parallel).
    assert!(!env_flag("PI_OFFLINE_MISSING_FOR_TEST"));
}

// ---------------------------------------------------------------------------
// src/config/keybindings.rs
// ---------------------------------------------------------------------------

use pir::config::keybindings::{keybindings_path, load_keybindings_file, parse_keybindings};

#[test]
fn parse_valid_chords_and_skip_schema() {
    let loaded = parse_keybindings(
        r#"{
                "$schema": "https://example/keybindings.schema.json",
                "newline": "ctrl+enter",
                "historyPrevious": ["alt+up", "alt+down"],
                "app.session.new": "ctrl+n"
            }"#,
    );
    assert!(loaded.errors.is_empty());
    assert!(!loaded.migrated);
    assert_eq!(
        loaded.bindings.get("newline"),
        Some(&vec!["ctrl+enter".to_string()])
    );
    assert_eq!(
        loaded.bindings.get("historyPrevious"),
        Some(&vec!["alt+up".to_string(), "alt+down".to_string()])
    );
}

#[test]
fn invalid_chords_are_dropped_with_errors() {
    let loaded = parse_keybindings(r#"{ "abort": "ctrl+not-a-key", "exit": 42 }"#);
    assert!(!loaded.bindings.contains_key("abort"));
    assert_eq!(loaded.errors.len(), 2);
}

#[test]
fn legacy_names_migrate() {
    let loaded = parse_keybindings(r#"{ "submit": "ctrl+j", "interrupt": "esc" }"#);
    assert!(loaded.migrated);
    assert_eq!(
        loaded.bindings.get("tui.input.submit"),
        Some(&vec!["ctrl+j".to_string()])
    );
    assert_eq!(
        loaded.bindings.get("app.interrupt"),
        Some(&vec!["esc".to_string()])
    );
    assert!(!loaded.bindings.contains_key("submit"));
}

#[test]
fn legacy_name_is_skipped_when_modern_name_present() {
    let loaded = parse_keybindings(r#"{ "submit": "ctrl+o", "tui.input.submit": "ctrl+n" }"#);
    assert!(loaded.migrated);
    assert_eq!(
        loaded.bindings.get("tui.input.submit"),
        Some(&vec!["ctrl+n".to_string()])
    );
}

#[test]
fn load_keybindings_file_handles_missing_and_migrated_files() {
    let agent = std::env::temp_dir().join(format!(
        "pir-keybindings-tests-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&agent).expect("create agent dir");
    // Missing file: defaults, nothing written.
    let loaded = load_keybindings_file(&agent);
    assert!(loaded.bindings.is_empty());
    assert!(!keybindings_path(&agent).exists());
    // Legacy file: migrated bindings written back.
    std::fs::write(keybindings_path(&agent), r#"{ "submit": "ctrl+j" }"#).expect("write legacy");
    let loaded = load_keybindings_file(&agent);
    assert!(loaded.migrated);
    assert!(loaded.bindings.contains_key("tui.input.submit"));
    let rewritten = std::fs::read_to_string(keybindings_path(&agent)).expect("read back");
    assert!(rewritten.contains("tui.input.submit"));
    let _ = std::fs::remove_dir_all(&agent);
}

// ---------------------------------------------------------------------------
// src/config/mcp.rs
// ---------------------------------------------------------------------------

use pir::config::mcp::{parse_mcp_config, McpServerConfig};

#[test]
fn parse_stdio_and_http_servers() {
    let (servers, errors) = parse_mcp_config(
        r#"{
                "fs": { "command": "npx", "args": ["-y", "fs-mcp"] },
                "web": { "url": "https://mcp.example.com/sse" }
            }"#,
    );
    assert!(errors.is_empty());
    assert_eq!(servers.len(), 2);
    assert!(matches!(servers["fs"], McpServerConfig::Stdio(_)));
    match &servers["web"] {
        McpServerConfig::Http(config) => {
            assert_eq!(config.url, "https://mcp.example.com/sse");
            assert!(servers["web"].is_enabled());
        }
        _ => panic!("web must be http"),
    }
}

#[test]
fn legacy_sse_type_is_rejected() {
    let (servers, errors) =
        parse_mcp_config(r#"{ "old": { "type": "sse", "url": "https://x/y" } }"#);
    assert!(servers.is_empty());
    assert!(errors.iter().any(|error| error.contains("SSE")));
}

#[test]
fn invalid_server_names_are_rejected() {
    let (servers, errors) = parse_mcp_config(r#"{ "bad name!": { "command": "x" } }"#);
    assert!(servers.is_empty());
    assert!(errors.iter().any(|error| error.contains("name must match")));
}

#[test]
fn malformed_json_records_an_error() {
    let (servers, errors) = parse_mcp_config("{ nope");
    assert!(servers.is_empty());
    assert_eq!(errors.len(), 1);
}

// ---------------------------------------------------------------------------
// src/config/settings.rs
// ---------------------------------------------------------------------------

use pir::config::settings::{
    apply_tool_modifiers, deep_merge_settings_values, get_tool_list_error, is_tool_modifier,
    resolve_default_tools, with_settings_file_lock, CacheWarmingMode, DefaultProjectTrust,
    FollowUpMode, Settings, SettingsManager, SettingsScope, SteeringMode, TransportSetting,
};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-settings-tests-{}-{}",
        name,
        uuid::Uuid::new_v4().simple()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, contents).expect("write file");
}

#[test]
fn tool_modifier_helpers_match_upstream() {
    assert!(is_tool_modifier("+codemode"));
    assert!(is_tool_modifier("-write"));
    assert!(!is_tool_modifier("read"));
    assert_eq!(
        get_tool_list_error(&["read".into(), "-write".into()]),
        Some("tool names cannot be mixed with +name or -name entries".to_string())
    );
    assert_eq!(
        get_tool_list_error(&["-write*".into()]),
        Some("+name and -name entries take exact tool names, not patterns: -write*".to_string())
    );
    assert_eq!(get_tool_list_error(&["read".into()]), None);
}

#[test]
fn apply_tool_modifiers_adds_and_removes() {
    let base: Vec<String> = ["read", "bash"]
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    let entries: Vec<String> = ["+edit", "-bash", "+read", "-missing"]
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    assert_eq!(apply_tool_modifiers(&base, &entries), vec!["read", "edit"]);
}

#[test]
fn resolve_default_tools_replaces_on_plain_names() {
    let entries: Vec<String> = ["read", "+grep"]
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    assert_eq!(resolve_default_tools(&entries), vec!["read", "grep"]);
    let modifiers = vec!["+grep".to_string()];
    assert_eq!(
        resolve_default_tools(&modifiers),
        vec!["read", "bash", "edit", "write", "grep"]
    );
}

#[test]
fn deep_merge_merges_objects_and_replaces_arrays() {
    let mut base =
        serde_json::json!({ "compaction": { "enabled": true, "reserveTokens": 1 }, "list": [1] });
    let over = serde_json::json!({ "compaction": { "reserveTokens": 2 }, "list": [9] });
    deep_merge_settings_values(&mut base, &over);
    assert_eq!(base["compaction"]["enabled"], serde_json::json!(true));
    assert_eq!(base["compaction"]["reserveTokens"], serde_json::json!(2));
    assert_eq!(base["list"], serde_json::json!([9]));
}

#[test]
fn default_tools_modifiers_append_across_layers() {
    let mut base = serde_json::json!({ "defaultTools": ["read", "bash"] });
    let over = serde_json::json!({ "defaultTools": ["-bash", "+edit"] });
    deep_merge_settings_values(&mut base, &over);
    assert_eq!(
        base["defaultTools"],
        serde_json::json!(["read", "bash", "-bash", "+edit"])
    );
    // Plain names still replace.
    let mut base = serde_json::json!({ "defaultTools": ["read"] });
    let over = serde_json::json!({ "defaultTools": ["grep"] });
    deep_merge_settings_values(&mut base, &over);
    assert_eq!(base["defaultTools"], serde_json::json!(["grep"]));
}

#[test]
fn migrations_run_on_load() {
    let dir = temp_dir("migrations");
    write_file(
        &dir.join("settings.json"),
        r#"{
                "queueMode": "all",
                "websockets": true,
                "skills": { "enableSkillCommands": false, "customDirectories": ["./sk"] },
                "retry": { "maxDelayMs": 4242 }
            }"#,
    );
    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    let raw = manager.raw.clone();
    assert_eq!(raw["steeringMode"], serde_json::json!("all"));
    assert!(raw.get("queueMode").is_none());
    assert_eq!(raw["transport"], serde_json::json!("websocket"));
    assert!(raw.get("websockets").is_none());
    assert_eq!(raw["skills"], serde_json::json!(["./sk"]));
    assert_eq!(raw["enableSkillCommands"], serde_json::json!(false));
    assert_eq!(
        raw["retry"]["provider"]["maxRetryDelayMs"],
        serde_json::json!(4242)
    );
    assert!(raw["retry"].get("maxDelayMs").is_none());
    assert!(manager.drain_errors().is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn unknown_settings_are_preserved_round_trip() {
    let dir = temp_dir("unknown-keys");
    write_file(
        &dir.join("settings.json"),
        r#"{ "theme": "dark", "extensionSetting": { "enabled": true } }"#,
    );
    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    assert_eq!(manager.settings.theme.as_deref(), Some("dark"));
    assert_eq!(
        manager.settings.extra.get("extensionSetting"),
        Some(&serde_json::json!({ "enabled": true }))
    );
    // A field-level write preserves the unknown key.
    manager.set_global_field("theme", serde_json::json!("light"));
    let on_disk = fs::read_to_string(dir.join("settings.json")).expect("read back");
    let value: serde_json::Value = serde_json::from_str(&on_disk).expect("json");
    assert_eq!(
        value["extensionSetting"]["enabled"],
        serde_json::json!(true)
    );
    assert_eq!(value["theme"], serde_json::json!("light"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn parse_errors_are_recorded_not_fatal() {
    let dir = temp_dir("parse-errors");
    write_file(&dir.join("settings.json"), "{ not json");
    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    assert_eq!(manager.settings, Settings::default());
    let errors = manager.drain_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].scope, SettingsScope::Global);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn project_scope_is_trust_gated_and_remerges() {
    let global = temp_dir("trust-global");
    let project = temp_dir("trust-project");
    write_file(&global.join("settings.json"), r#"{ "theme": "dark" }"#);
    write_file(&project.join("settings.json"), r#"{ "theme": "light" }"#);
    let mut manager = SettingsManager::load(&global, &project, &project, false);
    // Untrusted: project settings do not apply.
    assert_eq!(manager.theme(), Some("dark"));
    // Trusting reloads and re-merges (project wins).
    manager.set_project_trusted(true);
    assert_eq!(manager.theme(), Some("light"));
    // Untrusting clears project values again.
    manager.set_project_trusted(false);
    assert_eq!(manager.theme(), Some("dark"));
    let _ = fs::remove_dir_all(&global);
    let _ = fs::remove_dir_all(&project);
}

#[test]
fn field_level_persistence_writes_only_modified_fields() {
    let dir = temp_dir("persist");
    write_file(
        &dir.join("settings.json"),
        r#"{ "compaction": { "enabled": true, "reserveTokens": 1024 }, "theme": "dark" }"#,
    );
    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    manager.set_global_nested_field("compaction", "enabled", serde_json::json!(false));
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("settings.json")).expect("read back"))
            .expect("json");
    // Nested-key granularity: only the modified nested key changed.
    assert_eq!(value["compaction"]["enabled"], serde_json::json!(false));
    assert_eq!(
        value["compaction"]["reserveTokens"],
        serde_json::json!(1024)
    );
    assert_eq!(value["theme"], serde_json::json!("dark"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn project_writes_refuse_when_untrusted() {
    let dir = temp_dir("refuse");
    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, false);
    manager.persist_field(SettingsScope::Project, "theme", serde_json::json!("light"));
    let errors = manager.drain_errors();
    assert!(errors
        .iter()
        .any(|error| error.message.contains("not trusted")));
    assert!(!dir.join(".pi").join("settings.json").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn typed_accessors_apply_upstream_defaults() {
    let dir = temp_dir("accessors");
    let manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    assert_eq!(manager.steering_mode(), SteeringMode::OneAtATime);
    assert_eq!(manager.follow_up_mode(), FollowUpMode::OneAtATime);
    assert_eq!(manager.transport(), TransportSetting::Auto);
    assert!(manager.compaction_enabled());
    assert_eq!(manager.compaction_reserve_tokens(None), 16_384);
    assert_eq!(manager.compaction_keep_recent_tokens(None), 20_000);
    // Per-model overrides resolve before the ordinary setting.
    let with_overrides = temp_dir("accessors-overrides");
    write_file(
        &with_overrides.join("settings.json"),
        r#"{ "compaction": { "reserveTokens": 1024, "modelOverrides": { "anthropic/claude": { "reserveTokens": 8 } } } }"#,
    );
    let manager = SettingsManager::load(
        &with_overrides,
        &with_overrides.join(".pi"),
        &with_overrides,
        true,
    );
    assert_eq!(
        manager.compaction_reserve_tokens(Some(("anthropic", "claude"))),
        8
    );
    assert_eq!(manager.compaction_reserve_tokens(None), 1024);
    let retry = manager.retry_settings();
    assert_eq!(retry.enabled, Some(true));
    assert_eq!(retry.max_retries, Some(3));
    assert_eq!(retry.base_delay_ms, Some(2_000));
    assert_eq!(retry.max_agent_delay_ms, Some(60_000));
    assert_eq!(
        retry
            .provider
            .and_then(|provider| provider.max_retry_delay_ms),
        Some(60_000)
    );
    // Unset falls back to the upstream http-dispatcher default (5 minutes);
    // an explicit number wins, `"disabled"` and `0` map to 0 (upstream
    // `getHttpIdleTimeoutMs`).
    assert_eq!(manager.http_idle_timeout_ms(), 300_000);
    let disabled = temp_dir("idle-disabled");
    write_file(
        &disabled.join("settings.json"),
        r#"{ "httpIdleTimeoutMs": "disabled" }"#,
    );
    let manager = SettingsManager::load(&disabled, &disabled.join(".pi"), &disabled, true);
    assert_eq!(manager.http_idle_timeout_ms(), 0);
    let zero = temp_dir("idle-zero");
    write_file(&zero.join("settings.json"), r#"{ "httpIdleTimeoutMs": 0 }"#);
    let manager = SettingsManager::load(&zero, &zero.join(".pi"), &zero, true);
    assert_eq!(manager.http_idle_timeout_ms(), 0);
    let explicit = temp_dir("idle-explicit");
    write_file(
        &explicit.join("settings.json"),
        r#"{ "httpIdleTimeoutMs": 45000 }"#,
    );
    let manager = SettingsManager::load(&explicit, &explicit.join(".pi"), &explicit, true);
    assert_eq!(manager.http_idle_timeout_ms(), 45_000);
    assert_eq!(manager.cache_warming_mode(), CacheWarmingMode::Streaming);
    assert_eq!(manager.default_project_trust(), DefaultProjectTrust::Ask);
    assert!(!manager.clear_on_shrink());
    assert!(manager.install_telemetry_enabled());
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&with_overrides);
}

#[test]
fn device_id_is_created_once_and_persisted() {
    let dir = temp_dir("device-id");
    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    let first = manager.get_or_create_device_id();
    let second = manager.get_or_create_device_id();
    assert_eq!(first, second);
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("settings.json")).expect("read back"))
            .expect("json");
    assert_eq!(value["deviceId"], serde_json::json!(first));
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// src/config/mod.rs
// ---------------------------------------------------------------------------

use pir::config::{Config, ToolExecutionMode};

/// `Config` is the print host's scheduler source. It maps the three knobs
/// upstream models (`toolExecution`, its parallel ceiling and `failFast`) and
/// deliberately leaves the scheduler-only `call_timeout` unset: upstream has
/// no scheduler-level timeout setting, so a built-in host must not invent one.
#[test]
fn scheduler_config_maps_the_tool_fields_and_leaves_call_timeout_unset() {
    let config = Config {
        tool_execution: ToolExecutionMode::Sequential,
        max_parallel_tools: 3,
        fail_fast: true,
        allow_parallel_side_effects: true,
        ..Config::default()
    };
    let scheduler = config.to_scheduler_config();
    assert_eq!(
        scheduler.mode,
        pir::agent::tool_batch::ToolExecutionMode::Sequential
    );
    assert_eq!(scheduler.max_parallel, 3);
    assert!(scheduler.fail_fast);
    assert!(scheduler.allow_parallel_side_effects);
    assert_eq!(
        scheduler.call_timeout, None,
        "no upstream setting exists, so the built-in host leaves the embedding knob unset"
    );
}

/// The non-interactive turn cap: 1000 by default (one turn is one model
/// request; a message's tool calls are one batch), positive-integer
/// `PIR_MAX_TURNS` overrides, everything else ignored. The lookup seam keeps
/// the test off process-wide env mutation.
#[test]
fn max_turns_defaults_to_1000_and_accepts_positive_env_overrides() {
    use pir::config::env::{max_turns_from_lookup, MAX_TURNS_ENV};

    assert_eq!(pir::config::defaults::DEFAULT_AGENT_MAX_TURNS, 1_000);
    assert_eq!(
        pir::config::defaults::INTERACTIVE_AGENT_MAX_TURNS,
        100,
        "the TUI's upstream-semantics default stays at the original 100"
    );
    assert_eq!(Config::default().max_turns, 1_000);

    // The serde default (a persisted config without the field) is the same.
    let from_json: Config = serde_json::from_str("{}").expect("empty config");
    assert_eq!(from_json.max_turns, 1_000);

    fn lookup(value: Option<&str>) -> impl Fn(&str) -> Option<String> {
        let value = value.map(str::to_owned);
        move |name| {
            if name == MAX_TURNS_ENV {
                value.clone()
            } else {
                None
            }
        }
    }

    assert_eq!(max_turns_from_lookup(lookup(None)), 1_000);
    assert_eq!(max_turns_from_lookup(lookup(Some("2500"))), 2_500);
    assert_eq!(
        max_turns_from_lookup(lookup(Some(" 3000 "))),
        3_000,
        "surrounding whitespace is tolerated"
    );
    for invalid in ["0", "-1", "abc", ""] {
        assert_eq!(
            max_turns_from_lookup(lookup(Some(invalid))),
            1_000,
            "{invalid:?} must keep the safe default instead of disabling the guard"
        );
    }
}

// ---------------------------------------------------------------------------
// src/config/trust.rs
// ---------------------------------------------------------------------------

use pir::config::trust::{
    has_trust_requiring_project_resources, normalize_cwd, read_trust_file, resolve_project_trusted,
    ProjectTrustResolution, ProjectTrustStore,
};

/// Strip the Windows `\\?\` prefix the way `normalize_cwd` does.
fn strip_extended_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().into_owned();
    PathBuf::from(text.trim_start_matches(r"\\?\"))
}

fn temp_trust_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-trust-tests-{}-{}",
        name,
        uuid::Uuid::new_v4().simple()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn nearest_ancestor_lookup() {
    let agent = temp_trust_dir("agent");
    let root = temp_trust_dir("repo");
    let nested = root.join("a").join("b");
    fs::create_dir_all(&nested).expect("create nested");
    let store = ProjectTrustStore::new(&agent);
    // Mark only the repo root trusted.
    store.set(&root, Some(true)).expect("write trust");
    let entry = store.get(&nested).expect("nearest ancestor");
    let expected = strip_extended_prefix(fs::canonicalize(&root).expect("canonical"));
    assert_eq!(entry.path, expected);
    assert!(entry.decision);
    // No decision anywhere: None.
    let other = temp_trust_dir("other");
    assert!(ProjectTrustStore::new(&agent).get(&other).is_none());
    let _ = fs::remove_dir_all(&agent);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&other);
}

#[test]
fn set_many_removes_null_decisions_and_sorts_keys() {
    let agent = temp_trust_dir("agent");
    let a = temp_trust_dir("a");
    let b = temp_trust_dir("b");
    let store = ProjectTrustStore::new(&agent);
    store
        .set_many(vec![
            (a.clone(), Some(false)),
            (b.clone(), Some(true)),
            (a.clone(), None),
        ])
        .expect("write trust");
    let data = read_trust_file(store.path()).expect("read");
    assert!(data.contains_key(&normalize_cwd(&b).to_string_lossy().into_owned()));
    assert!(!data.contains_key(&normalize_cwd(&a).to_string_lossy().into_owned()));
    let _ = fs::remove_dir_all(&agent);
    let _ = fs::remove_dir_all(&a);
    let _ = fs::remove_dir_all(&b);
}

#[test]
fn trust_requires_project_resources() {
    let project = temp_trust_dir("project");
    // No .pi resources: implicitly trusted.
    assert!(!has_trust_requiring_project_resources(&project));
    fs::create_dir_all(project.join(".pi")).expect("create .pi");
    fs::write(project.join(".pi").join("settings.json"), "{}").expect("write");
    assert!(has_trust_requiring_project_resources(&project));
    let _ = fs::remove_dir_all(&project);
}

#[test]
fn resolution_order_follows_upstream() {
    let agent = temp_trust_dir("agent");
    let project = temp_trust_dir("project");
    fs::create_dir_all(project.join(".pi")).expect("create .pi");
    fs::write(project.join(".pi").join("settings.json"), "{}").expect("write");
    let store = ProjectTrustStore::new(&agent);

    // Override wins.
    assert_eq!(
        resolve_project_trusted(
            &project,
            &store,
            Some(true),
            DefaultProjectTrust::Never,
            false
        ),
        ProjectTrustResolution::Resolved(true)
    );
    // Stored decision next.
    store.set(&project, Some(true)).expect("store");
    assert_eq!(
        resolve_project_trusted(&project, &store, None, DefaultProjectTrust::Never, false),
        ProjectTrustResolution::Resolved(true)
    );
    // defaultProjectTrust=never beats ask when nothing is stored.
    store.set(&project, None).expect("clear");
    assert_eq!(
        resolve_project_trusted(&project, &store, None, DefaultProjectTrust::Never, true),
        ProjectTrustResolution::Resolved(false)
    );
    assert_eq!(
        resolve_project_trusted(&project, &store, None, DefaultProjectTrust::Always, false),
        ProjectTrustResolution::Resolved(true)
    );
    // ask: selector with UI, not-trusted headless.
    assert_eq!(
        resolve_project_trusted(&project, &store, None, DefaultProjectTrust::Ask, false),
        ProjectTrustResolution::Resolved(false)
    );
    assert_eq!(
        resolve_project_trusted(&project, &store, None, DefaultProjectTrust::Ask, true),
        ProjectTrustResolution::Ask
    );
    let _ = fs::remove_dir_all(&agent);
    let _ = fs::remove_dir_all(&project);
}

#[test]
fn project_scope_cannot_supply_device_or_tracking_ids() {
    let global = temp_dir("ids-global");
    let project = temp_dir("ids-project");
    write_file(
        &project.join("settings.json"),
        r#"{ "deviceId": "project-id", "trackingId": "project-tracking", "enableAnalytics": true }"#,
    );
    let mut manager = SettingsManager::load(&global, &project, &project, true);

    assert!(
        !manager.enable_analytics(),
        "a project settings file must not opt the user into analytics"
    );
    assert_eq!(
        manager.tracking_id(),
        None,
        "a project settings file must not supply the analytics id"
    );
    let id = manager.get_or_create_device_id();
    assert_ne!(id, "project-id");
    let persisted: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(global.join("settings.json")).expect("global"))
            .expect("json");
    assert_eq!(persisted["deviceId"], serde_json::json!(id));
    let _ = fs::remove_dir_all(&global);
    let _ = fs::remove_dir_all(&project);
}

#[test]
fn reload_drops_errors_from_a_previous_broken_file() {
    let dir = temp_dir("reload-errors");
    write_file(&dir.join("settings.json"), "{ not json");
    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, true);
    assert_eq!(manager.drain_errors().len(), 1);
    // A second load records the same failure again; fixing the file and
    // reloading must not keep the stale diagnostic.
    manager.reload();
    assert_eq!(manager.drain_errors().len(), 1);
    write_file(&dir.join("settings.json"), r#"{ "theme": "light" }"#);
    manager.reload();
    assert!(
        manager.drain_errors().is_empty(),
        "a successful reload must clear stale load errors"
    );
    assert_eq!(manager.theme(), Some("light"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn settings_file_lock_is_released_when_the_closure_panics() {
    let dir = temp_dir("lock-panic");
    let path = dir.join("settings.json");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_settings_file_lock(&path, || panic!("boom"));
    }));
    assert!(result.is_err());
    assert!(
        !dir.join("settings.json.lock").exists(),
        "a panicking lock holder must release the lock"
    );
    with_settings_file_lock(&path, || fs::write(&path, "{}").expect("write"));
    assert_eq!(fs::read_to_string(&path).expect("read"), "{}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn settings_file_lock_takes_over_an_abandoned_lock() {
    use std::time::{Duration, SystemTime};

    let dir = temp_dir("lock-stale");
    let path = dir.join("settings.json");
    let lock = dir.join("settings.json.lock");
    fs::write(&lock, "crashed-owner").expect("stale lock");
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&lock)
        .expect("open stale lock");
    file.set_modified(SystemTime::now() - Duration::from_secs(11))
        .expect("age the stale lock");
    drop(file);
    with_settings_file_lock(&path, || fs::write(&path, "{}").expect("write"));
    assert!(
        !lock.exists(),
        "the stale lock must be taken over and released"
    );
    let _ = fs::remove_dir_all(&dir);
}
