//! Generated configuration schema gate (upstream `test/config-schemas.test.ts`):
//! the committed `schemas/*.schema.json` artifacts must match the schemas
//! built from the typed settings/models/keybindings definitions, and
//! representative documents must validate.
//!
//! Regenerate stale artifacts with:
//! `cargo test --test config_schemas regenerate_committed_artifacts -- --ignored`

use std::path::PathBuf;

use serde_json::Value;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_committed(relative: &str) -> Value {
    let path = repo_root().join(relative);
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("missing committed schema {relative}: {error}"));
    serde_json::from_str(&contents).expect("committed schema must be valid JSON")
}

fn assert_fresh(relative: &str, generated: &Value) {
    let committed = read_committed(relative);
    assert_eq!(
        &committed, generated,
        "{relative} is stale; run `cargo test --test config_schemas regenerate_committed_artifacts -- --ignored`"
    );
}

#[test]
fn committed_schemas_match_generated() {
    for (relative, generated) in pir::config::schema::render_config_schemas() {
        assert_fresh(relative, &generated);
    }
}

#[test]
fn schemas_use_draft_2020_12_and_stable_ids() {
    for (_, schema) in pir::config::schema::render_config_schemas() {
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        let id = schema["$id"].as_str().expect("$id");
        assert!(id.ends_with(".schema.json"), "$id {id}");
        assert!(
            id.contains("settings")
                || id.contains("models")
                || id.contains("keybindings")
                || id.contains("theme"),
        );
    }
}

#[test]
fn settings_schema_preserves_upstream_guidance() {
    let settings = pir::config::schema::settings_schema();
    let properties = &settings["properties"];
    assert!(properties["quietStartup"]["description"]
        .as_str()
        .expect("description")
        .contains("hide all startup output"));
    assert!(properties["defaultTools"]["description"]
        .as_str()
        .expect("description")
        .contains("+name and -name"));
    assert!(properties["fullscreenWheelScrollLines"]["description"]
        .as_str()
        .expect("description")
        .contains("1 to 100"));
    // Legacy migrations stay documented.
    assert!(
        properties["retry"]["properties"]["maxDelayMs"]["description"]
            .as_str()
            .expect("description")
            .contains("provider.maxRetryDelayMs")
    );
    assert_eq!(
        properties["queueMode"]["description"],
        "Legacy setting migrated to steeringMode."
    );
    // D31 (`cea257439`): the share destination carries the upstream
    // description, `auto` default and three-value domain.
    let share = &properties["sessionShareDestination"];
    assert_eq!(
        share["description"],
        "Destination used by /share. \"auto\" uses Radius when authenticated and otherwise creates a GitHub gist."
    );
    assert_eq!(share["default"], "auto");
    assert_eq!(share["enum"], serde_json::json!(["auto", "gist", "radius"]));
}

#[test]
fn settings_schema_describes_the_full_upstream_surface() {
    let schema = pir::config::schema::settings_schema();
    let properties = schema["properties"].as_object().expect("properties");
    for field in [
        "$schema",
        "lastChangelogVersion",
        "defaultProvider",
        "defaultModel",
        "defaultThinkingLevel",
        "modelThinkingLevels",
        "transport",
        "sessionShareDestination",
        "steeringMode",
        "followUpMode",
        "theme",
        "compaction",
        "branchSummary",
        "retry",
        "hideThinkingBlock",
        "showCacheMissNotices",
        "externalEditor",
        "shellPath",
        "quietStartup",
        "defaultProjectTrust",
        "shellCommandPrefix",
        "npmCommand",
        "collapseChangelog",
        "enableInstallTelemetry",
        "enableAnalytics",
        "trackingId",
        "packages",
        "extensions",
        "deviceId",
        "skills",
        "prompts",
        "themes",
        "enableSkillCommands",
        "terminal",
        "images",
        "enabledModels",
        "defaultTools",
        "doubleEscapeAction",
        "treeFilterMode",
        "thinkingBudgets",
        "editorPaddingX",
        "outputPad",
        "autocompleteMaxVisible",
        "showHardwareCursor",
        "markdown",
        "warnings",
        "codemode",
        "extensionLimits",
        "sessionDir",
        "httpProxy",
        "httpIdleTimeoutMs",
        "cacheWarming",
        "websocketConnectTimeoutMs",
        "tuiMode",
        "fullscreenExitOutput",
        "fullscreenScrollbar",
        "fullscreenCopyOnSelect",
        "fullscreenWheelScrollLines",
        "queueMode",
        "websockets",
    ] {
        assert!(
            properties.contains_key(field),
            "settings schema omitted {field}"
        );
    }
}

#[test]
fn theme_schema_tracks_upstream() {
    let theme = pir::config::schema::theme_schema();
    assert_eq!(theme["title"], "Pi Coding Agent Theme");
    let required = theme["required"].as_array().expect("required");
    assert_eq!(required.len(), 2);
    assert!(required.contains(&serde_json::json!("name")));
    assert!(required.contains(&serde_json::json!("colors")));
    let colors = &theme["properties"]["colors"];
    for field in [
        "accent",
        "selectedBg",
        "thinkingMax",
        "bashMode",
        "syntaxKeyword",
    ] {
        assert!(colors["properties"].get(field).is_some(), "missing {field}");
    }
    // Optional colors fall back instead of being required.
    assert!(!colors["required"]
        .as_array()
        .expect("colors.required")
        .contains(&serde_json::json!("thinkingMax")));
    assert!(theme["$defs"]["ColorValue"].is_object());
    // The upstream reference URL points at the coding-agent schemas dir.
    assert!(theme["$id"]
        .as_str()
        .expect("$id")
        .ends_with("/theme.schema.json"));
}

/// Regenerate the committed artifacts. `#[ignore]`d by default so the normal
/// test run is read-only.
#[test]
#[ignore]
fn regenerate_committed_artifacts() {
    for (relative, schema) in pir::config::schema::render_config_schemas() {
        let path = repo_root().join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create schemas dir");
        }
        let serialized = serde_json::to_string_pretty(&schema).expect("serialize schema");
        std::fs::write(&path, format!("{serialized}\n")).expect("write schema");
    }
}
