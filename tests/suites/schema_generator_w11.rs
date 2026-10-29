//! W11 generator ⇄ committed-schema agreement (lane W12-J).
//!
//! W11-E corrected the committed artifacts under `schemas/`. The Rust
//! generator in `src/config/schema.rs` is what must reproduce them: the
//! freshness gates (`tests/suites/config_schemas.rs::
//! committed_schemas_match_generated` and `tests/upstream_w2_drift.rs::
//! d24_committed_schemas_carry_the_published_ids`) compare each rendered
//! `serde_json::Value` against the parsed committed file. This suite adds the
//! per-schema detail so a regression names the exact property that drifted,
//! instead of dumping a whole-schema equality failure:
//!
//! - every rendered schema equals the committed `Value` (the gate itself),
//! - `models`: the root/`$defs`/`patternProperties` shape mirrors upstream,
//!   the `ProviderCompat` `$defs` entry carries the same 45 properties and the
//!   four enum knobs, `compat` is a `$ref` in all three places,
//!   `models[].required` is `["id"]` while `modelOverrides` entries stay
//!   unconstrained,
//! - `keybindings`: 90 action ids as `properties`, `additionalProperties` and
//!   `$defs.KeybindingValue` preserved, key-id regex escaped,
//! - `settings`: the seven documented defaults plus
//!   `extensionLimits.beforeAgentStartTimeoutMs`,
//! - `theme`: the two color descriptions that differ from the obvious
//!   wording.
//!
//! NOTE: this file is registered by the foreman (`#[path = "suites/
//! schema_generator_w11.rs"]` in one of the `g_*` merged binaries).

use std::path::PathBuf;

use serde_json::Value;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The committed artifact, parsed the same way the freshness gates parse it.
fn committed(relative: &str) -> Value {
    let path = repo_root().join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("missing committed schema {relative}: {error}"));
    serde_json::from_str(&text).expect("committed schema is valid JSON")
}

/// The rendered artifacts, keyed by their repository-relative path.
fn generated() -> Vec<(&'static str, Value)> {
    pir::config::schema::render_config_schemas()
}

fn rendered(name: &str) -> Value {
    generated()
        .into_iter()
        .find(|(relative, _)| relative.ends_with(name))
        .unwrap_or_else(|| panic!("render_config_schemas omitted {name}"))
        .1
}

// ---------------------------------------------------------------------------
// The freshness gate, schema by schema
// ---------------------------------------------------------------------------

/// Every rendered artifact equals its committed file. This is exactly what
/// `config_schemas::committed_schemas_match_generated` and
/// `upstream_w2_drift::d24_committed_schemas_carry_the_published_ids` assert;
/// running it per file makes a failure name the file first.
#[test]
fn w11_rendered_schemas_match_committed_artifacts() {
    for (relative, generated) in generated() {
        let committed = committed(relative);
        assert_eq!(
            generated, committed,
            "{relative}: the generator no longer reproduces the committed artifact"
        );
    }
}

/// The four committed files are the only artifacts rendered, so a new schema
/// cannot be added to the generator without a committed counterpart (and vice
/// versa) unnoticed.
#[test]
fn w11_generator_renders_exactly_the_committed_artifacts() {
    let mut rendered: Vec<&str> = generated().iter().map(|(path, _)| *path).collect();
    rendered.sort_unstable();
    assert_eq!(
        rendered,
        vec![
            "schemas/keybindings.schema.json",
            "schemas/models.schema.json",
            "schemas/settings.schema.json",
            "schemas/theme.schema.json",
        ]
    );
}

// ---------------------------------------------------------------------------
// models.schema.json
// ---------------------------------------------------------------------------

/// `ProviderCompat` is a `$defs` entry with the upstream property set. The
/// committed file carries 45 knobs; a missing one silently widens
/// `additionalProperties` validation for that key.
#[test]
fn w11_models_compat_defs_carry_the_full_provider_compat_surface() {
    let models = rendered("models.schema.json");
    let compat = &models["$defs"]["ProviderCompat"];
    assert_eq!(compat["type"], "object");
    assert_eq!(compat["additionalProperties"], true);

    let properties = compat["properties"]
        .as_object()
        .expect("ProviderCompat.properties");
    assert_eq!(properties.len(), 45, "ProviderCompat property count");
    for field in [
        "supportsStore",
        "maxTokensField",
        "thinkingFormat",
        "sessionAffinityFormat",
        "cacheControlFormat",
        "thinkingTokenBudgetField",
        "vllmPriority",
        "allowedFallbackModels",
        "inputLimits",
        "promptCache",
    ] {
        assert!(
            properties.contains_key(field),
            "ProviderCompat omits {field}"
        );
    }

    // The four enum knobs keep their upstream domains.
    let enum_values = |field: &str| -> Vec<String> {
        properties[field]["anyOf"]
            .as_array()
            .unwrap_or_else(|| panic!("{field} must be an anyOf const union"))
            .iter()
            .map(|member| {
                member["const"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{field} member without const"))
                    .to_string()
            })
            .collect()
    };
    assert_eq!(
        enum_values("thinkingFormat"),
        vec![
            "openai",
            "openrouter",
            "deepseek",
            "together",
            "baseten",
            "zai",
            "qwen",
            "chat-template",
            "qwen-chat-template",
            "string-thinking",
            "ant-ling",
        ]
    );
    assert_eq!(
        enum_values("sessionAffinityFormat"),
        vec!["openai", "openai-nosession", "openrouter"]
    );
    assert_eq!(
        enum_values("thinkingTokenBudgetField"),
        vec![
            "thinking_token_budget",
            "thinking_budget",
            "thinking_budget_tokens"
        ]
    );
    assert_eq!(properties["cacheControlFormat"]["const"], "anthropic");
}

/// Both provider-level and model-level `compat` blocks point at the shared
/// definition; an inline object there would fork the contract.
#[test]
fn w11_models_compat_is_a_ref_everywhere_it_appears() {
    let models = rendered("models.schema.json");
    let provider = &models["properties"]["providers"]["patternProperties"]["^.*$"]["properties"];
    assert_eq!(
        provider["compat"]["$ref"], "#/$defs/ProviderCompat",
        "provider-level compat"
    );
    assert_eq!(
        provider["models"]["items"]["properties"]["compat"]["$ref"], "#/$defs/ProviderCompat",
        "models[].compat"
    );
    assert_eq!(
        provider["modelOverrides"]["patternProperties"]["^.*$"]["properties"]["compat"]["$ref"],
        "#/$defs/ProviderCompat",
        "modelOverrides[].compat"
    );
}

/// A `models[]` entry requires an id; a `modelOverrides` entry is keyed by its
/// id and stays unconstrained, so a partial override is valid.
#[test]
fn w11_models_require_id_only_on_model_array_entries() {
    let models = rendered("models.schema.json");
    let provider = &models["properties"]["providers"]["patternProperties"]["^.*$"]["properties"];
    assert_eq!(
        provider["models"]["items"]["required"],
        serde_json::json!(["id"])
    );
    assert!(
        provider["modelOverrides"]["patternProperties"]["^.*$"]
            .get("required")
            .is_none(),
        "modelOverrides entries must not require any field"
    );
}

/// The published artifact carries the upstream root/defs shape: a `$comment`
/// instead of `title`/`description`/`additionalProperties: false`, provider and
/// nested maps through `patternProperties`, and the four shared `$defs` that
/// pir previously inlined.
#[test]
fn w11_models_schema_matches_the_upstream_shape() {
    let models = rendered("models.schema.json");
    assert_eq!(
        models["$comment"],
        "This file is generated from TypeScript source. Do not edit it manually; run npm run generate:schemas."
    );
    for absent in ["title", "description", "additionalProperties"] {
        assert!(
            models.get(absent).is_none(),
            "root must not publish {absent} (upstream leaves unknown keys open)"
        );
    }

    let providers = &models["properties"]["providers"];
    assert!(providers.get("additionalProperties").is_none());
    assert_eq!(
        providers["patternProperties"]["^.*$"]["type"], "object",
        "the provider map is a patternProperties map"
    );
    let provider = &providers["patternProperties"]["^.*$"]["properties"];
    let map = "headers";
    assert!(provider[map].get("additionalProperties").is_none());
    assert_eq!(
        provider[map]["patternProperties"]["^.*$"]["type"], "string",
        "{map} values are strings"
    );
    assert!(provider["modelOverrides"]
        .get("additionalProperties")
        .is_none());
    assert_eq!(
        provider["modelOverrides"]["patternProperties"]["^.*$"]["type"],
        "object"
    );

    let items = &provider["models"]["items"]["properties"];
    assert_eq!(
        items["samplingParams"]["patternProperties"]["^.*$"],
        serde_json::json!({}),
        "samplingParams publishes the free-form value schema"
    );
    for level in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
        assert_eq!(
            items["samplingParamsByThinkingLevel"]["properties"][level]["patternProperties"]
                ["^.*$"],
            serde_json::json!({}),
            "{level} uses patternProperties"
        );
    }

    let defs = models["$defs"].as_object().expect("$defs");
    for name in [
        "ModelCost",
        "ModelInputLimits",
        "ModelPromptCache",
        "ProviderCompat",
        "ThinkingLevelMap",
    ] {
        assert!(defs.contains_key(name), "$defs omits {name}");
    }
    assert_eq!(
        models["$defs"]["ModelCost"]["required"],
        serde_json::json!(["input", "output", "cacheRead", "cacheWrite"]),
        "models[].cost is a complete rate table"
    );
    assert_eq!(items["cost"]["$ref"], "#/$defs/ModelCost");
    assert_eq!(
        models["$defs"]["ProviderCompat"]["properties"]["allowedFallbackModels"]["items"]
            ["properties"]["cost"]["$ref"],
        "#/$defs/ModelCost"
    );
    assert_eq!(
        items["thinkingLevelMap"]["$ref"],
        "#/$defs/ThinkingLevelMap"
    );
    assert_eq!(
        models["$defs"]["ThinkingLevelMap"]["properties"]
            .as_object()
            .expect("ThinkingLevelMap.properties")
            .len(),
        7
    );
    for token_field in ["contextWindow", "maxTokens"] {
        assert_eq!(items[token_field]["type"], "number", "{token_field}");
        assert_eq!(items[token_field]["exclusiveMinimum"], 0, "{token_field}");
    }

    // apiKey is the upstream string template; pir's runtime is Option<String>.
    assert_eq!(provider["apiKey"]["type"], "string");
    assert_eq!(provider["apiKey"]["minLength"], 1);
    // oauth keeps the upstream const but also declares its type.
    assert_eq!(provider["oauth"]["type"], "string");
    assert_eq!(provider["oauth"]["const"], "radius");
    // Upstream-only compat knobs pir does not read stay undocumented instead
    // of being advertised as supported (recorded in the NOTES file).
    for absent in ["imageInput", "sendImageOptInHeader"] {
        assert!(
            models["$defs"]["ProviderCompat"]["properties"]
                .get(absent)
                .is_none(),
            "ProviderCompat must not advertise the unsupported {absent}"
        );
    }
}

// ---------------------------------------------------------------------------
// keybindings.schema.json
// ---------------------------------------------------------------------------

/// Every bindable action id is documented, while unknown ids stay valid (the
/// loader ignores them) through `additionalProperties`.
#[test]
fn w11_keybindings_list_every_action_and_keep_the_permissive_fallthrough() {
    let keybindings = rendered("keybindings.schema.json");
    let properties = keybindings["properties"]
        .as_object()
        .expect("keybindings.properties");
    let actions: Vec<&String> = properties
        .keys()
        .filter(|key| key.as_str() != "$schema")
        .collect();
    assert_eq!(actions.len(), 90, "keybinding action id count");
    assert_eq!(
        keybindings["additionalProperties"]["$ref"],
        "#/$defs/KeybindingValue"
    );
    for action in [
        "tui.editor.cursorUp",
        "app.interrupt",
        "app.tree.filter.all",
    ] {
        assert_eq!(
            properties[action]["$ref"], "#/$defs/KeybindingValue",
            "{action} must reference the shared value definition"
        );
        assert!(
            properties[action]["description"].is_string(),
            "{action} must carry its upstream description"
        );
    }
    assert_eq!(
        keybindings["$defs"]["KeybindingValue"]["anyOf"]
            .as_array()
            .expect("KeybindingValue.anyOf")
            .len(),
        2,
        "a chord or an array of chords"
    );
}

/// The key-id pattern escapes regex metacharacters: named keys such as `[`
/// and `.` must match literally, and duplicate modifiers are rejected.
#[test]
fn w11_keybindings_key_id_pattern_is_regex_escaped() {
    let keybindings = rendered("keybindings.schema.json");
    let pattern = keybindings["$defs"]["KeybindingValue"]["anyOf"][0]["pattern"]
        .as_str()
        .expect("key-id pattern");
    for escaped in [
        r"\[", r"\]", r"\.", r"\$", r"\(", r"\)", r"\*", r"\+", r"\|",
    ] {
        assert!(
            pattern.contains(escaped),
            "key-id pattern must escape {escaped} (pattern: {pattern})"
        );
    }
    assert!(
        pattern.contains(r"\\"),
        "the backslash key must be escaped as a literal backslash"
    );
    assert!(
        pattern.starts_with("^(?!.*(?:"),
        "duplicate-modifier lookahead must be present"
    );
    assert!(pattern.ends_with('$'), "pattern must be anchored");
}

// ---------------------------------------------------------------------------
// settings.schema.json
// ---------------------------------------------------------------------------

/// The seven booleans whose runtime defaults live in
/// `config::defaults::settings_defaults` are published with those defaults.
#[test]
fn w11_settings_publish_the_documented_boolean_defaults() {
    let settings = rendered("settings.schema.json");
    let properties = &settings["properties"];
    for (path, expected) in [
        (vec!["compaction", "enabled"], true),
        (vec!["hideThinkingBlock"], false),
        (vec!["showCacheMissNotices"], false),
        (vec!["collapseChangelog"], false),
        (vec!["enableInstallTelemetry"], true),
        (vec!["enableAnalytics"], false),
        (vec!["enableSkillCommands"], true),
    ] {
        let mut node = properties;
        for step in &path[..path.len() - 1] {
            node = &node[*step]["properties"];
        }
        let field = &node[path[path.len() - 1]];
        assert_eq!(field["default"], expected, "{} default", path.join("."));
    }
}

/// `extensionLimits` documents the `before_agent_start` dispatch budget next
/// to the other QuickJS limits.
#[test]
fn w11_settings_extension_limits_include_before_agent_start_budget() {
    let settings = rendered("settings.schema.json");
    let limits = &settings["properties"]["extensionLimits"]["properties"];
    for field in [
        "memoryMb",
        "stackMb",
        "gcThresholdMb",
        "callTimeoutMs",
        "hookTimeoutMs",
        "beforeAgentStartTimeoutMs",
    ] {
        assert!(limits.get(field).is_some(), "extensionLimits omits {field}");
        assert_eq!(limits[field]["type"], "number");
        assert_eq!(limits[field]["minimum"], 0);
    }
    let description = limits["beforeAgentStartTimeoutMs"]["description"]
        .as_str()
        .expect("description");
    assert!(
        description.contains("before_agent_start"),
        "the budget must name the lifecycle hook"
    );
    assert!(
        description.contains("10 s"),
        "the 0-means-default note must name the 10 s default"
    );
}

// ---------------------------------------------------------------------------
// theme.schema.json
// ---------------------------------------------------------------------------

/// Two color descriptions are not the obvious wording; both must be the
/// committed strings exactly.
#[test]
fn w11_theme_color_descriptions_match_the_committed_wording() {
    let theme = rendered("theme.schema.json");
    let colors = &theme["properties"]["colors"]["properties"];
    assert_eq!(
        colors["userMessageText"]["description"],
        "User message text"
    );
    assert_eq!(
        colors["customMessageText"]["description"],
        "Custom message text"
    );
}
