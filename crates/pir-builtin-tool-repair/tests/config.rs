//! Tests for the package config surface: normalization, the agent-dir-relative
//! path, and file loading (including unreadable/malformed fallbacks).

use std::path::Path;

use serde_json::json;

use pir_builtin_tool_repair::aliases::GRAMMAR_NAMES;
use pir_builtin_tool_repair::config::{
    GrammarRepairConfig, GrammarRepairMode, ToolRepairConfig, CONFIG_FILE_NAME,
};

#[test]
fn the_grammar_block_accepts_the_envelope_and_the_bare_shape() {
    let envelope = GrammarRepairConfig::from_json(&json!({
        "grammarRepair": {
            "enabled": true,
            "grammars": ["glm", "bogus", "qwen"],
            "mode": "strip",
            "requireKnownTool": false,
            "debug": true,
            "leakModels": ["glm", "(", "kimi"]
        }
    }));
    assert!(envelope.enabled);
    assert_eq!(envelope.grammars, vec!["glm", "qwen"]);
    assert_eq!(envelope.mode, GrammarRepairMode::Strip);
    assert!(!envelope.require_known_tool);
    assert!(envelope.debug);
    assert_eq!(envelope.leak_models, vec!["glm", "kimi"]);

    let bare = GrammarRepairConfig::from_json(&json!({"enabled": true}));
    assert!(bare.enabled);
    assert_eq!(bare.grammars, GRAMMAR_NAMES.to_vec());
    assert_eq!(bare.mode, GrammarRepairMode::Recover);
}

#[test]
fn missing_or_ill_typed_fields_fall_back_to_the_package_defaults() {
    let defaults = GrammarRepairConfig::default();
    let config = GrammarRepairConfig::from_json(&json!({
        "grammarRepair": {
            "enabled": "yes",
            "grammars": "glm",
            "mode": "recover",
            "requireKnownTool": 1,
            "debug": null,
            "leakModels": "glm"
        }
    }));
    assert_eq!(config, defaults);

    // All-unknown grammar names count as "no valid names" and keep the
    // defaults rather than disabling the pass.
    let unknown = GrammarRepairConfig::from_json(&json!({"grammars": ["nope"]}));
    assert_eq!(unknown.grammars, GRAMMAR_NAMES.to_vec());
}

#[test]
fn the_config_path_is_agent_dir_relative() {
    let path = ToolRepairConfig::config_path_in(Path::new("/tmp/agent"));
    assert!(path.ends_with(Path::new("extensions").join(CONFIG_FILE_NAME)));
    assert_eq!(
        path,
        Path::new("/tmp/agent")
            .join("extensions")
            .join(CONFIG_FILE_NAME)
    );
}

#[test]
fn a_missing_config_file_keeps_the_defaults() {
    let dir = std::env::temp_dir().join(format!("pir-tool-repair-missing-{}", std::process::id()));
    let config = ToolRepairConfig::load_from(&dir.join("does-not-exist.json"));
    assert_eq!(config.grammar, GrammarRepairConfig::default());
}

#[test]
fn a_package_config_file_is_loaded_and_normalized() {
    let dir = std::env::temp_dir().join(format!("pir-tool-repair-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        r#"{"grammarRepair":{"enabled":true,"grammars":["dsml"],"leakModels":["glm"]}}"#,
    )
    .expect("write config");

    let config = ToolRepairConfig::load_from(&path);
    assert!(config.grammar.enabled);
    assert_eq!(config.grammar.grammars, vec!["dsml"]);
    assert_eq!(config.grammar.leak_models, vec!["glm"]);

    std::fs::write(&path, "{ not json").expect("write malformed config");
    let malformed = ToolRepairConfig::load_from(&path);
    assert_eq!(malformed.grammar, GrammarRepairConfig::default());

    let _ = std::fs::remove_dir_all(&dir);
}
