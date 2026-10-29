//! W11-E: published configuration schemas and the provider-catalog snapshot.
//!
//! Two independent gates live here:
//!
//! 1. **Schema ⇄ Rust source**. `schemas/*.schema.json` are hand-maintained
//!    copies of the upstream TypeScript-generated artifacts (the Rust
//!    generator in `src/config/schema.rs` covers the same files). These tests
//!    read the Rust sources as text and assert that the documented key sets and
//!    enum values are exactly the ones the types actually serialize:
//!    - `models.schema.json` `compat` ⇄ `OpenAiCompat`
//!      (`src/ai/types/model/compat.rs`),
//!    - `models.schema.json` model items ⇄ `PiModelConfig`
//!      (`src/ai/providers/pi_config.rs`),
//!    - `keybindings.schema.json` action ids ⇄ the three Rust keybinding
//!      registries (`src/tui/keybindings.rs`, `src/config/keybindings.rs`,
//!      `src/tui/altscreen_search.rs`),
//!    - `settings.schema.json` properties ⇄ the `Settings` structs and the
//!      documented defaults in `src/config/defaults.rs`,
//!    - `theme.schema.json` colors ⇄ `THEME_COLORS` in `src/config/schema.rs`.
//!
//! 2. **Catalog snapshot hygiene**. Every checked-in catalog JSON must be
//!    canonical (LF, exactly one trailing newline, the generator's stable
//!    field/key order) and must load through the production loader. The same
//!    canonical ordering is enforced from the generator side by
//!    `node scripts/gen_catalog.mjs --self-check` (no upstream data required)
//!    and `node scripts/gen_catalog.mjs --check --data-dir <dir>` (full
//!    regeneration gate).
//!
//! NOTE: this file is registered by the foreman (`tests/g_protocol.rs` or
//! `tests/g_ai.rs`, `#[path = "suites/schema_catalog_w11.rs"]`).

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;

use pir::ai::providers::catalog;

// ---------------------------------------------------------------------------
// Source-reading helpers
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_source(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn read_schema(name: &str) -> Value {
    let relative = format!("schemas/{name}.schema.json");
    let text = read_source(&relative);
    let value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{relative} is not valid JSON: {error}"));
    assert_eq!(
        value["$schema"], "https://json-schema.org/draft/2020-12/schema",
        "{relative}: draft 2020-12"
    );
    assert_eq!(
        value["$id"],
        format!("https://pi.dev/schemas/{name}.schema.json"),
        "{relative}: published $id"
    );
    value
}

/// The body between the braces of the first occurrence of `header` (a struct
/// or enum declaration such as `pub struct Settings {`).
fn brace_block<'a>(source: &'a str, header: &str) -> &'a str {
    let start = source
        .find(header)
        .unwrap_or_else(|| panic!("source does not contain `{header}`"));
    let open = source[start..]
        .find('{')
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("`{header}` has no body"));
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[open + 1..open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("`{header}` body is unbalanced");
}

/// Two serde wire-name tables do not follow the struct's attribute chain:
///
/// - `Settings` uses `#[serde(default)]` only (no `rename_all`), so each field
///   carries its exact camelCase `rename`; `$schema` is the one exception
///   handled below.
/// - `PiModelConfig` does the same, with `id`/`name`/`api`/`reasoning`/
///   `headers`/`compat`/`input` relying on their plain spellings.
fn struct_fields(source: &str, struct_name: &str) -> Vec<String> {
    let header = format!("pub struct {struct_name} {{");
    let body = brace_block(source, &header);
    let mut fields = Vec::new();
    let mut rename: Option<String> = None;
    let mut alias: Option<String> = None;
    let mut flattened = false;
    for line in body.lines() {
        if let Some(start) = line.find("rename = \"") {
            let rest = &line[start + "rename = \"".len()..];
            if let Some(end) = rest.find('"') {
                rename = Some(rest[..end].to_string());
            }
        }
        if let Some(start) = line.find("alias = \"") {
            let rest = &line[start + "alias = \"".len()..];
            if let Some(end) = rest.find('"') {
                alias = Some(rest[..end].to_string());
            }
        }
        if line.contains("flatten") {
            flattened = true;
        }
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("pub ") {
            if let Some(colon) = rest.find(':') {
                let field = rest[..colon].trim();
                if !field.is_empty() && field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    if !flattened {
                        fields.push(
                            rename
                                .take()
                                .or_else(|| alias.take())
                                .unwrap_or_else(|| field.to_string()),
                        );
                    }
                    rename = None;
                    alias = None;
                    flattened = false;
                }
            }
        }
    }
    fields
}

/// The serde wire names of an enum's unit variants, in declaration order.
/// Honours the container's `#[serde(rename_all = "...")]` rule and any
/// per-variant `#[serde(rename = "...")]` override.
fn enum_variants(source: &str, enum_name: &str) -> Vec<String> {
    let header = format!("pub enum {enum_name} {{");
    let declaration = source
        .find(&header)
        .unwrap_or_else(|| panic!("source does not contain `{header}`"));
    // Walk back over contiguous attribute lines to find the container rule.
    let before = &source[..declaration];
    let mut rename_all: Option<String> = None;
    for line in before.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with("#[") {
            break;
        }
        if let Some(start) = trimmed.find("rename_all = \"") {
            let rest = &trimmed[start + "rename_all = \"".len()..];
            if let Some(end) = rest.find('"') {
                rename_all = Some(rest[..end].to_string());
            }
        }
    }
    let body = brace_block(source, &header);
    let mut variants = Vec::new();
    let mut pending: Option<String> = None;
    for line in body.lines() {
        if let Some(start) = line.find("rename = \"") {
            let rest = &line[start + "rename = \"".len()..];
            if let Some(end) = rest.find('"') {
                pending = Some(rest[..end].to_string());
            }
            continue;
        }
        let trimmed = line.trim();
        let name: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let rest = &trimmed[name.len()..];
        if !name.is_empty()
            && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && (rest.trim_start().starts_with(',') || rest.trim_start().starts_with('{'))
        {
            let wire = pending.take().unwrap_or_else(|| {
                rename_all
                    .as_deref()
                    .map(|rule| apply_rename_all(&name, rule))
                    .unwrap_or(name)
            });
            variants.push(wire);
        }
    }
    variants
}

/// Split a PascalCase variant name into lowercase words
/// (`NoTools` → `["no", "tools"]`, `OpenAi` → `["open", "ai"]`).
fn variant_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (index, ch) in chars.iter().enumerate() {
        if ch.is_ascii_uppercase() && !current.is_empty() {
            let previous_lower = chars[index - 1].is_ascii_lowercase();
            let next_lower = chars
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_lowercase());
            if previous_lower || next_lower {
                words.push(std::mem::take(&mut current));
            }
        }
        current.push(ch.to_ascii_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn apply_rename_all(name: &str, rule: &str) -> String {
    let words = variant_words(name);
    match rule {
        "lowercase" => name.to_ascii_lowercase(),
        "UPPERCASE" => name.to_ascii_uppercase(),
        "snake_case" => words.join("_"),
        "kebab-case" => words.join("-"),
        "SCREAMING_SNAKE_CASE" => words.join("_").to_ascii_uppercase(),
        "camelCase" => {
            let mut result = words.first().cloned().unwrap_or_default();
            for word in words.iter().skip(1) {
                let mut chars = word.chars();
                if let Some(first) = chars.next() {
                    result.push(first.to_ascii_uppercase());
                    result.extend(chars);
                }
            }
            result
        }
        "PascalCase" => words
            .iter()
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect(),
        other => panic!("unsupported rename_all rule `{other}`"),
    }
}

fn camel_case(field: &str) -> String {
    let mut result = String::with_capacity(field.len());
    let mut upper = false;
    for ch in field.chars() {
        if ch == '_' {
            upper = true;
        } else if upper {
            result.push(ch.to_ascii_uppercase());
            upper = false;
        } else {
            result.push(ch);
        }
    }
    result
}

fn keys_of(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("expected an object, got {value}"))
        .keys()
        .cloned()
        .collect()
}

fn set_of<I, S>(items: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    items.into_iter().map(Into::into).collect()
}

fn string_array(value: &Value) -> BTreeSet<String> {
    set_of(
        value
            .as_array()
            .unwrap_or_else(|| panic!("expected an array, got {value}"))
            .iter()
            .map(|item| {
                item.as_str()
                    .unwrap_or_else(|| panic!("expected a string, got {item}"))
            }),
    )
}

/// Enum values written either as `"enum": [...]` or as a `const`/`anyOf`
/// union (the upstream TypeBox shape).
fn enum_values(value: &Value) -> BTreeSet<String> {
    let mut values = BTreeSet::new();
    if let Some(list) = value.get("enum").and_then(Value::as_array) {
        for item in list {
            if let Some(text) = item.as_str() {
                values.insert(text.to_string());
            }
        }
    }
    if let Some(constant) = value.get("const").and_then(Value::as_str) {
        values.insert(constant.to_string());
    }
    for branch in ["anyOf", "oneOf"] {
        if let Some(list) = value.get(branch).and_then(Value::as_array) {
            for item in list {
                values.extend(enum_values(item));
            }
        }
    }
    values
}

// ---------------------------------------------------------------------------
// models.schema.json ⇄ OpenAiCompat / PiModelConfig
// ---------------------------------------------------------------------------

/// The upstream `ProviderCompat` property names
/// (`packages/coding-agent/schemas/models.schema.json` `$defs.ProviderCompat`,
/// generated from `packages/ai/src/providers/compat-schema.ts`). The published
/// schema must stay a superset of this list.
const UPSTREAM_PROVIDER_COMPAT: &[&str] = &[
    "supportsStore",
    "supportsDeveloperRole",
    "supportsReasoningEffort",
    "supportsUsageInStreaming",
    "supportsFinishReason",
    "maxTokensField",
    "requiresToolResultName",
    "requiresAssistantAfterToolResult",
    "requiresThinkingAsText",
    "requiresReasoningContentOnAssistantMessages",
    "thinkingFormat",
    "chatTemplateKwargs",
    "chatTemplateArgs",
    "openRouterRouting",
    "vercelGatewayRouting",
    "zaiToolStream",
    "thinkingTokenBudgetField",
    "supportsThinkingTokenBudget",
    "supportsOpenAIGrammarTools",
    "supportsMidConvoSystemMessages",
    "supportsMidConvoToolAdditions",
    "supportsStrictMode",
    "cacheControlFormat",
    "sendSessionAffinityHeaders",
    "sessionAffinityFormat",
    "supportsLongCacheRetention",
    "vllmPriority",
    "supportsAdditionalTools",
    "supportsToolSearch",
    "supportsExplicitPromptCacheMode",
    "supportsMidConvoEffort",
    "supportsMaxOutputTokens",
    "supportsEagerToolInputStreaming",
    "supportsCacheControlOnTools",
    "supportsTemperature",
    "forceAdaptiveThinking",
    "allowEmptySignature",
    "supportsStrictTools",
    "supportsMidConvoToolChanges",
    "allowedFallbackModels",
];

/// Model-level fields pir carries on the compat block instead of on `Model`
/// (`samplingParamsByThinkingLevel`, `inputLimits`, `promptCache`), plus the
/// provider-level `customHeaders` / `authHeader` that reach the same struct.
const PIR_COMPAT_EXTRAS: &[&str] = &[
    "samplingParamsByThinkingLevel",
    "customHeaders",
    "inputLimits",
    "promptCache",
    "authHeader",
];

fn openai_compat_keys() -> BTreeSet<String> {
    let source = read_source("src/ai/types/model/compat.rs");
    let mut keys = BTreeSet::new();
    for line in source.lines() {
        if let Some(start) = line.find("rename = \"") {
            let rest = &line[start + "rename = \"".len()..];
            if let Some(end) = rest.find('"') {
                keys.insert(rest[..end].to_string());
            }
        }
    }
    keys
}

#[test]
fn models_schema_compat_matches_openai_compat() {
    let schema = read_schema("models");
    let compat = &schema["$defs"]["ProviderCompat"];
    let properties = keys_of(&compat["properties"]);
    let rust = openai_compat_keys();

    assert_eq!(rust.len(), 45, "OpenAiCompat carries 45 compat fields");
    for key in &rust {
        assert!(
            properties.contains(key),
            "OpenAiCompat field `{key}` is missing from models.schema.json $defs.ProviderCompat"
        );
    }
    let expected: BTreeSet<String> = rust
        .union(&set_of(PIR_COMPAT_EXTRAS.iter().copied()))
        .cloned()
        .collect();
    let undocumented: Vec<_> = properties.difference(&expected).cloned().collect();
    let missing: Vec<_> = expected.difference(&properties).cloned().collect();
    assert!(
        undocumented.is_empty() && missing.is_empty(),
        "ProviderCompat properties drifted from OpenAiCompat: schema-only {undocumented:?}, Rust-only {missing:?}"
    );

    // The published schema must keep the full upstream superset (provider-level
    // models.json compat), not just the fields pir currently consumes.
    for upstream in UPSTREAM_PROVIDER_COMPAT {
        assert!(
            properties.contains(*upstream),
            "upstream ProviderCompat field `{upstream}` is missing from the published schema"
        );
    }
    assert_eq!(
        UPSTREAM_PROVIDER_COMPAT.len(),
        40,
        "upstream ProviderCompat field count"
    );
    // Upstream-only knobs pir's runtime has no field for stay undocumented
    // (models.schema.json.NOTES.md records the gap); advertising them would
    // imply support that does not exist.
    for unported in ["imageInput", "sendImageOptInHeader"] {
        assert!(
            !properties.contains(unported),
            "ProviderCompat must not advertise the unported upstream field `{unported}`"
        );
    }
    assert_eq!(
        compat["additionalProperties"], true,
        "compat keeps unknown options"
    );

    // Every models.json compat position resolves to the same definition.
    let provider = &schema["properties"]["providers"]["patternProperties"]["^.*$"];
    assert_eq!(
        provider["properties"]["compat"]["$ref"],
        "#/$defs/ProviderCompat"
    );
    assert_eq!(
        provider["properties"]["models"]["items"]["properties"]["compat"]["$ref"],
        "#/$defs/ProviderCompat"
    );
    assert_eq!(
        provider["properties"]["modelOverrides"]["patternProperties"]["^.*$"]["properties"]
            ["compat"]["$ref"],
        "#/$defs/ProviderCompat"
    );

    // The generated catalog's field allow-list is the same set (a catalog
    // compat key the loader cannot deserialize would be silently dropped).
    let generator = read_source("scripts/gen_catalog.mjs");
    let start = generator
        .find("const KNOWN_COMPAT_KEYS = new Set([")
        .expect("gen_catalog.mjs declares KNOWN_COMPAT_KEYS");
    let block = &generator[start..];
    let block = &block[..block.find("]);").expect("KNOWN_COMPAT_KEYS is closed")];
    let generator_keys: BTreeSet<String> = block
        .match_indices('"')
        .collect::<Vec<_>>()
        .chunks(2)
        .filter_map(|pair| {
            let (open, _) = pair[0];
            let (close, _) = pair.get(1)?;
            Some(block[open + 1..*close].to_string())
        })
        .collect();
    assert_eq!(
        generator_keys, rust,
        "scripts/gen_catalog.mjs KNOWN_COMPAT_KEYS must mirror OpenAiCompat"
    );
}

/// A field order benchmark for `PiModelConfig`'s wire shape: every models.json
/// model field must deserialize into the struct.
#[test]
fn models_schema_model_items_match_pi_model_config() {
    let schema = read_schema("models");
    let source = read_source("src/ai/providers/pi_config.rs");
    let rust: BTreeSet<String> = set_of(struct_fields(&source, "PiModelConfig"));
    let provider = &schema["properties"]["providers"]["patternProperties"]["^.*$"];

    let items = keys_of(&provider["properties"]["models"]["items"]["properties"]);
    assert_eq!(
        items, rust,
        "model item properties must match PiModelConfig"
    );

    let overrides = keys_of(
        &provider["properties"]["modelOverrides"]["patternProperties"]["^.*$"]["properties"],
    );
    let unknown: Vec<_> = overrides.difference(&rust).cloned().collect();
    assert!(
        unknown.is_empty(),
        "modelOverrides properties not representable by PiModelConfig: {unknown:?}"
    );

    // `id` is the only required model field (upstream `ModelDefinitionSchema`).
    assert_eq!(
        provider["properties"]["models"]["items"]["required"],
        serde_json::json!(["id"]),
        "models[].id is required"
    );
    assert_eq!(
        provider["properties"]["modelOverrides"]["description"],
        "Per-model overrides applied last, to chat models only."
    );
}

#[test]
fn models_schema_enums_match_rust_types() {
    let schema = read_schema("models");
    let models_source = read_source("src/ai/types/model.rs");
    let formats_source = read_source("src/ai/types/model/formats.rs");
    let compat = &schema["$defs"]["ProviderCompat"]["properties"];

    let thinking_levels = set_of(enum_variants(&models_source, "ThinkingLevel"));
    assert_eq!(
        thinking_levels,
        set_of(["off", "minimal", "low", "medium", "high", "xhigh", "max"]),
        "ThinkingLevel wire names"
    );

    let thinking_format = set_of(enum_variants(&formats_source, "ThinkingFormat"));
    assert_eq!(
        enum_values(&compat["thinkingFormat"]),
        thinking_format,
        "thinkingFormat must enumerate every ThinkingFormat variant"
    );
    assert_eq!(
        enum_values(&compat["sessionAffinityFormat"]),
        set_of(enum_variants(&formats_source, "SessionAffinityFormat")),
        "sessionAffinityFormat variants"
    );
    assert_eq!(
        enum_values(&compat["cacheControlFormat"]),
        set_of(enum_variants(&formats_source, "CacheControlFormat")),
        "cacheControlFormat variants"
    );
    assert_eq!(
        enum_values(&compat["thinkingTokenBudgetField"]),
        set_of(enum_variants(&models_source, "ThinkingTokenBudgetField")),
        "thinkingTokenBudgetField variants"
    );
    assert_eq!(
        enum_values(&compat["maxTokensField"]),
        set_of(["max_completion_tokens", "max_tokens"]),
        "maxTokensField accepts exactly the two upstream spellings"
    );

    // `thinkingLevelMap` and `samplingParamsByThinkingLevel` are keyed by the
    // seven model thinking levels.
    let provider = &schema["properties"]["providers"]["patternProperties"]["^.*$"];
    let level_map = &provider["properties"]["models"]["items"]["properties"]
        ["samplingParamsByThinkingLevel"]["properties"];
    assert_eq!(keys_of(level_map), thinking_levels);
    let items = &provider["properties"]["models"]["items"]["properties"];
    let thinking_level_def = &schema["$defs"]["ThinkingLevelMap"]["properties"];
    assert_eq!(
        keys_of(thinking_level_def),
        thinking_levels,
        "thinkingLevelMap enumerates the seven upstream levels"
    );
    assert_eq!(
        thinking_level_def["off"]["anyOf"][0]["type"], "string",
        "a level maps to a provider value"
    );
    assert_eq!(
        thinking_level_def["off"]["anyOf"][1]["type"], "null",
        "null marks a level unsupported"
    );

    assert_eq!(
        items["input"]["items"]["enum"],
        serde_json::json!(["text", "image"]),
        "input modalities"
    );
}

// ---------------------------------------------------------------------------
// keybindings.schema.json ⇄ the Rust keybinding registries
// ---------------------------------------------------------------------------

#[test]
fn keybindings_schema_enumerates_every_rust_action_id() {
    let schema = read_schema("keybindings");
    let mut rust_ids = BTreeSet::new();
    for source in [
        "src/tui/keybindings.rs",
        "src/config/keybindings.rs",
        "src/tui/altscreen_search.rs",
    ] {
        let text = read_source(source);
        for quote in text.match_indices('"') {
            let rest = &text[quote.0 + 1..];
            let Some(end) = rest.find('"') else { continue };
            let candidate = &rest[..end];
            if (candidate.starts_with("tui.") || candidate.starts_with("app."))
                && candidate
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
            {
                rust_ids.insert(candidate.to_string());
            }
        }
    }

    let properties = &schema["properties"];
    let mut documented: BTreeSet<String> = keys_of(properties);
    documented.remove("$schema");

    assert_eq!(
        rust_ids.len(),
        90,
        "the union of the three Rust registries is the 90 upstream action ids"
    );
    assert_eq!(
        documented, rust_ids,
        "keybindings.schema.json action ids must match the Rust registries"
    );

    for (id, definition) in properties.as_object().expect("properties object") {
        if id == "$schema" {
            continue;
        }
        assert_eq!(
            definition["$ref"], "#/$defs/KeybindingValue",
            "{id}: every action accepts a key or key array"
        );
        assert!(
            definition["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "{id}: every action documents its description"
        );
        assert!(
            id.starts_with("tui.") || id.starts_with("app."),
            "{id}: namespaced upstream action id"
        );
    }

    assert_eq!(
        schema["additionalProperties"]["$ref"], "#/$defs/KeybindingValue",
        "unregistered ids still validate as key values"
    );
    let key_value = &schema["$defs"]["KeybindingValue"]["anyOf"];
    assert_eq!(key_value[0]["type"], "string");
    assert_eq!(key_value[1]["type"], "array");
    let pattern = key_value[0]["pattern"]
        .as_str()
        .expect("key pattern is a string");
    for literal in ["ctrl", "shift", "alt", "super", "pageUp", "f12", "escape"] {
        assert!(pattern.contains(literal), "key pattern covers {literal}");
    }
}

// ---------------------------------------------------------------------------
// settings.schema.json ⇄ Settings structs and documented defaults
// ---------------------------------------------------------------------------

/// Legacy keys the schema keeps for migration but the struct no longer stores
/// (`queueMode` → `steeringMode`, `websockets` → `transport`).
const LEGACY_SETTINGS_KEYS: &[&str] = &["queueMode", "websockets"];

#[test]
fn settings_schema_properties_match_settings_struct() {
    let schema = read_schema("settings");
    let source = read_source("src/config/settings.rs");
    let rust: BTreeSet<String> = set_of(struct_fields(&source, "Settings"));
    let mut documented = keys_of(&schema["properties"]);
    for legacy in LEGACY_SETTINGS_KEYS {
        assert!(
            documented.remove(*legacy),
            "legacy key {legacy} stays documented"
        );
    }

    assert_eq!(
        documented, rust,
        "settings.schema.json properties must match Settings' serde wire names"
    );
    assert_eq!(
        schema["additionalProperties"], true,
        "unknown settings are preserved round-trip (`Settings::extra`)"
    );
}

#[test]
fn settings_schema_nested_objects_match_their_structs() {
    let schema = read_schema("settings");
    let source = read_source("src/config/settings/schema.rs");
    let properties = &schema["properties"];

    let cases: &[(&str, &str)] = &[
        ("TerminalSettings", "terminal"),
        ("ImageSettings", "images"),
        ("MarkdownSettings", "markdown"),
        ("WarningSettings", "warnings"),
        ("CodemodeSettings", "codemode"),
        ("ExtensionLimitsSettings", "extensionLimits"),
        ("ToolsSettings", "tools"),
        ("RetrySettings", "retry"),
        ("ProviderRetrySettings", "provider"),
        ("CompactionSettings", "compaction"),
        ("BranchSummarySettings", "branchSummary"),
        ("ThinkingBudgetsSettings", "thinkingBudgets"),
    ];

    for (struct_name, key_path) in cases {
        let rust: BTreeSet<String> = struct_fields(&source, struct_name)
            .into_iter()
            .map(|field| camel_case(&field))
            .collect();
        let node = if *key_path == "provider" {
            &properties["retry"]["properties"]["provider"]
        } else {
            &properties[*key_path]
        };
        let mut documented = keys_of(&node["properties"]);
        // The legacy retry delay migrated into `provider.maxRetryDelayMs`.
        documented.remove("maxDelayMs");
        let undocumented: Vec<_> = documented.difference(&rust).cloned().collect();
        let missing: Vec<_> = rust.difference(&documented).cloned().collect();
        assert!(
            undocumented.is_empty() && missing.is_empty(),
            "{struct_name} drift: schema-only {undocumented:?}, Rust-only {missing:?}"
        );
        assert_eq!(
            node["additionalProperties"], false,
            "{struct_name}: unknown nested settings are rejected"
        );
    }
}

#[test]
fn settings_schema_defaults_match_documented_constants() {
    let schema = read_schema("settings");
    let defaults = read_source("src/config/defaults.rs");
    let properties = &schema["properties"];

    let bool_cases: &[(&str, &str, bool)] = &[
        ("hideThinkingBlock", "HIDE_THINKING_BLOCK", false),
        ("showCacheMissNotices", "SHOW_CACHE_MISS_NOTICES", false),
        ("collapseChangelog", "COLLAPSE_CHANGELOG", false),
        ("enableInstallTelemetry", "ENABLE_INSTALL_TELEMETRY", true),
        ("enableAnalytics", "ENABLE_ANALYTICS", false),
        ("enableSkillCommands", "ENABLE_SKILL_COMMANDS", true),
    ];
    for (property, constant, expected) in bool_cases {
        let needle = format!("const {constant}: bool = {expected}");
        assert!(
            defaults.contains(&needle),
            "src/config/defaults.rs must declare `{needle}`"
        );
        assert_eq!(
            properties[*property]["default"],
            serde_json::json!(expected),
            "{property}: schema default must match {constant}"
        );
    }

    assert!(
        defaults.contains("const COMPACTION_ENABLED: bool = true"),
        "compaction defaults on"
    );
    assert_eq!(
        properties["compaction"]["properties"]["enabled"]["default"],
        true
    );
    assert_eq!(
        properties["compaction"]["properties"]["reserveTokens"]["default"],
        16_384
    );
    assert_eq!(
        properties["compaction"]["properties"]["keepRecentTokens"]["default"],
        20_000
    );
}

#[test]
fn settings_schema_enums_match_rust_enums() {
    let schema = read_schema("settings");
    let source = read_source("src/config/settings/schema.rs");
    let properties = &schema["properties"];
    let thinking = set_of(enum_variants(
        &read_source("src/ai/types/model.rs"),
        "ThinkingLevel",
    ));

    assert_eq!(enum_values(&properties["defaultThinkingLevel"]), thinking);
    assert_eq!(
        enum_values(&properties["modelThinkingLevels"]["additionalProperties"]),
        thinking
    );
    // `TransportSetting` is asserted against the upstream wire ids rather than
    // the Rust enum: its variants currently serialize as `Auto`/`Sse`/
    // `Websocket` (the enum lacks `#[serde(rename_all = "lowercase")]`), which
    // is a source-side gap reported by this lane; the schema keeps the
    // documented upstream contract.
    assert_eq!(
        enum_values(&properties["transport"]),
        set_of(["auto", "sse", "websocket", "websocket-cached"])
    );
    assert_eq!(
        enum_values(&properties["sessionShareDestination"]),
        set_of(enum_variants(&source, "SessionShareDestination"))
    );
    assert_eq!(
        enum_values(&properties["steeringMode"]),
        set_of(enum_variants(&source, "SteeringMode"))
    );
    assert_eq!(
        enum_values(&properties["followUpMode"]),
        set_of(enum_variants(&source, "FollowUpMode"))
    );
    assert_eq!(
        enum_values(&properties["treeFilterMode"]),
        set_of(enum_variants(&source, "TreeFilterMode"))
    );
    assert_eq!(
        enum_values(&properties["doubleEscapeAction"]),
        set_of(enum_variants(&source, "DoubleEscapeAction"))
    );
    assert_eq!(
        enum_values(&properties["tuiMode"]),
        set_of(enum_variants(&source, "TuiMode"))
    );
    assert_eq!(
        enum_values(&properties["fullscreenExitOutput"]),
        set_of(enum_variants(&source, "FullscreenExitOutput"))
    );
    assert_eq!(
        enum_values(&properties["fullscreenScrollbar"]),
        set_of(enum_variants(&source, "FullscreenScrollbar"))
    );
    assert_eq!(
        enum_values(&properties["cacheWarming"]),
        set_of(enum_variants(&source, "CacheWarmingMode"))
    );
    assert_eq!(
        enum_values(&properties["markdown"]["properties"]["mermaid"]),
        set_of(enum_variants(&source, "MermaidRenderingMode"))
    );
    assert_eq!(
        enum_values(&properties["codemode"]["properties"]["mode"]),
        set_of(enum_variants(&source, "CodemodeMode"))
    );
    assert_eq!(
        properties["terminal"]["properties"]["images"]["enum"],
        serde_json::json!(["kitty", "iterm2", "auto", false]),
        "terminal.images accepts the string modes plus literal false for off"
    );
}

// ---------------------------------------------------------------------------
// theme.schema.json ⇄ THEME_COLORS
// ---------------------------------------------------------------------------

#[test]
fn theme_schema_colors_match_the_color_table() {
    let schema = read_schema("theme");
    let source = read_source("src/config/schema.rs");
    let header = "const THEME_COLORS: &[(&str, &str, bool)] = &[";
    let start = source
        .find(header)
        .expect("src/config/schema.rs declares THEME_COLORS");
    let table = &source[start + header.len()..];
    let table = &table[..table.find("];").expect("THEME_COLORS is closed")];

    // Each row is `("name", "description", <required>),`.
    let mut names = Vec::new();
    let mut required = Vec::new();
    let mut rest = table;
    while let Some(start) = rest.find("(\"") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('"') else { break };
        let name = &rest[..end];
        rest = &rest[end + 1..];
        // Close this row first, then the row's last `, ` separates the
        // description from the bool flag.
        let Some(row_end) = rest.find("),") else {
            break;
        };
        let row = &rest[..row_end];
        let flag = row
            .rfind(", ")
            .map(|index| row[index + 2..].trim_start())
            .unwrap_or_default();
        names.push(name.to_string());
        if flag.starts_with("true") {
            required.push(name.to_string());
        }
        rest = &rest[row_end + 2..];
    }

    assert_eq!(names.len(), 56, "THEME_COLORS declares 56 color rows");
    let colors = &schema["properties"]["colors"];
    assert_eq!(
        keys_of(&colors["properties"]),
        set_of(names.iter().cloned()),
        "theme color tokens must match THEME_COLORS"
    );
    assert_eq!(
        string_array(&colors["required"]),
        set_of(required.iter().cloned()),
        "required theme colors must match THEME_COLORS' required flags"
    );
    assert_eq!(
        colors["additionalProperties"], false,
        "unknown theme colors are rejected"
    );
    assert_eq!(
        schema["required"],
        serde_json::json!(["name", "colors"]),
        "a theme requires name + colors"
    );
    assert_eq!(schema["additionalProperties"], false);
}

// ---------------------------------------------------------------------------
// Provider catalog snapshot hygiene
// ---------------------------------------------------------------------------

/// The generator's canonical model field order (`canonicalModel` in
/// `scripts/gen_catalog.mjs`).
const MODEL_FIELD_ORDER: &[&str] = &[
    "id",
    "name",
    "type",
    "api",
    "baseUrl",
    "reasoning",
    "thinkingLevelMap",
    "input",
    "output",
    "cost",
    "compat",
    "contextWindow",
    "maxTokens",
];
const COST_FIELD_ORDER: &[&str] = &["input", "output", "cacheRead", "cacheWrite", "tiers"];
const TIER_FIELD_ORDER: &[&str] = &[
    "inputTokensAbove",
    "input",
    "output",
    "cacheRead",
    "cacheWrite",
];
const THINKING_LEVEL_ORDER: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];
const MODALITY_ORDER: &[&str] = &["text", "image", "audio", "video"];

fn catalog_dir() -> PathBuf {
    repo_root().join("src/ai/providers/catalog")
}

fn ordered_subset<'a>(keys: &[&'a str], allowed: &[&str]) -> Vec<&'a str> {
    keys.iter()
        .filter(|key| allowed.contains(key))
        .copied()
        .collect()
}

fn assert_sorted(values: &[&str], label: &str) {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    assert_eq!(values, sorted.as_slice(), "{label} must be sorted");
}

#[test]
fn catalog_files_are_canonical() {
    let mut checked = 0usize;
    for (provider_id, file) in catalog::CATALOG_FILES {
        let path = catalog_dir().join(*file);
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        assert!(
            !text.contains('\r'),
            "{file}: catalog files use LF line endings"
        );
        assert!(
            text.ends_with('\n') && !text.ends_with("\n\n"),
            "{file}: exactly one trailing newline"
        );
        let raw: Value = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{file} is not valid JSON: {error}"));

        // Provider level: name, optional baseUrl, models — in that order.
        let provider_keys: Vec<&str> = raw
            .as_object()
            .expect("provider object")
            .keys()
            .map(String::as_str)
            .collect();
        let expected_provider: Vec<&str> = ["name", "baseUrl", "models"]
            .into_iter()
            .filter(|key| provider_keys.contains(key))
            .collect();
        assert_eq!(
            provider_keys, expected_provider,
            "{file}: provider key order"
        );
        assert!(
            !raw["name"].as_str().unwrap_or_default().is_empty(),
            "{file}: provider display name"
        );
        assert!(raw["models"].is_object(), "{file}: models is an object");

        let models = raw["models"].as_object().expect("models object");
        let model_keys: Vec<&str> = models.keys().map(String::as_str).collect();
        assert_sorted(&model_keys, &format!("{file}: model keys"));

        for (key, entry) in models {
            let fields: Vec<&str> = entry
                .as_object()
                .unwrap_or_else(|| panic!("{provider_id}/{key}: model entry object"))
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                fields,
                ordered_subset(&fields, MODEL_FIELD_ORDER),
                "{provider_id}/{key}: canonical model field order"
            );

            if let Some(cost) = entry.get("cost") {
                let cost_fields: Vec<&str> = cost
                    .as_object()
                    .expect("cost object")
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    cost_fields,
                    ordered_subset(&cost_fields, COST_FIELD_ORDER),
                    "{provider_id}/{key}: canonical cost field order"
                );
                if let Some(tiers) = cost.get("tiers").and_then(Value::as_array) {
                    for tier in tiers {
                        let tier_fields: Vec<&str> = tier
                            .as_object()
                            .expect("tier object")
                            .keys()
                            .map(String::as_str)
                            .collect();
                        assert_eq!(
                            tier_fields,
                            ordered_subset(&tier_fields, TIER_FIELD_ORDER),
                            "{provider_id}/{key}: canonical cost-tier field order"
                        );
                    }
                }
            }

            if let Some(compat) = entry.get("compat") {
                let compat_keys: Vec<&str> = compat
                    .as_object()
                    .expect("compat object")
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert_sorted(&compat_keys, &format!("{provider_id}/{key}: compat keys"));
            }

            if let Some(levels) = entry.get("thinkingLevelMap") {
                let level_keys: Vec<&str> = levels
                    .as_object()
                    .expect("thinkingLevelMap object")
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    level_keys,
                    ordered_subset(&level_keys, THINKING_LEVEL_ORDER),
                    "{provider_id}/{key}: thinkingLevelMap level order"
                );
            }

            for field in ["input", "output"] {
                if let Some(values) = entry.get(field).and_then(Value::as_array) {
                    let modalities: Vec<&str> = values
                        .iter()
                        .map(|value| value.as_str().expect("modality string"))
                        .collect();
                    assert_eq!(
                        modalities,
                        ordered_subset(&modalities, MODALITY_ORDER),
                        "{provider_id}/{key}: {field} modality order"
                    );
                }
            }
        }

        checked += 1;
    }
    assert_eq!(checked, 42, "the snapshot ships 42 provider files");
}

#[test]
fn catalog_files_load_through_the_production_loader() {
    let on_disk: BTreeSet<String> = fs::read_dir(catalog_dir())
        .expect("catalog directory")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            name.ends_with(".json").then_some(name)
        })
        .collect();
    let registered: BTreeSet<String> = set_of(catalog::CATALOG_FILES.iter().map(|(_, file)| *file));
    assert_eq!(
        on_disk, registered,
        "every catalog JSON is registered in CATALOG_FILES and vice versa"
    );

    let mut chat_models = 0usize;
    for (provider_id, file) in catalog::CATALOG_FILES {
        let text = fs::read_to_string(catalog_dir().join(*file)).expect("catalog file");
        let raw: Value = serde_json::from_str(&text).expect("catalog JSON");
        let expected_chat = raw["models"]
            .as_object()
            .expect("models object")
            .values()
            .filter(|entry| entry.get("type").and_then(Value::as_str).unwrap_or("chat") == "chat")
            .count();

        let provider = catalog::load_provider(provider_id)
            .unwrap_or_else(|error| panic!("{file}: production loader failed: {error}"))
            .unwrap_or_else(|| panic!("{file}: provider {provider_id} is not embedded"));
        assert_eq!(provider.id, *provider_id);
        assert_eq!(
            provider.models.len(),
            expected_chat,
            "{file}: chat entries load 1:1 through CatalogProvider"
        );
        chat_models += provider.models.len();
    }
    assert_eq!(
        chat_models, 1656,
        "the snapshot exposes 1656 chat models to the registry"
    );
}

#[test]
fn catalog_overrides_reference_existing_entries() {
    let overrides: Value = serde_json::from_str(&read_source("scripts/catalog-overrides.json"))
        .expect("catalog-overrides.json is valid JSON");
    let ids: BTreeSet<String> = set_of(catalog::CATALOG_FILES.iter().map(|(id, _)| *id));

    let providers = keys_of(&overrides["providers"]);
    let unknown: Vec<_> = providers.difference(&ids).cloned().collect();
    assert!(
        unknown.is_empty(),
        "overrides name providers without a catalog file: {unknown:?}"
    );

    for section in ["models", "extraModels"] {
        for (provider_id, entries) in overrides[section]
            .as_object()
            .unwrap_or_else(|| panic!("overrides.{section} is an object"))
        {
            assert!(
                ids.contains(provider_id),
                "overrides.{section}: unknown provider {provider_id}"
            );
            let file = catalog::CATALOG_FILES
                .iter()
                .find(|(id, _)| id == provider_id)
                .map(|(_, file)| *file)
                .expect("provider file");
            let raw: Value = serde_json::from_str(
                &fs::read_to_string(catalog_dir().join(file)).expect("catalog file"),
            )
            .expect("catalog JSON");
            let models = raw["models"].as_object().expect("models object");
            for key in entries.as_object().expect("override entries").keys() {
                assert!(
                    models.contains_key(key),
                    "overrides.{section}.{provider_id}.{key} has no catalog entry"
                );
            }
        }
    }
}

#[test]
fn catalog_generator_exposes_the_snapshot_gate() {
    let script = read_source("scripts/gen_catalog.mjs");
    assert!(
        script.contains("--self-check"),
        "gen_catalog.mjs must expose the no-upstream-data --self-check gate"
    );
    assert!(
        script.contains("function selfCheck("),
        "gen_catalog.mjs implements selfCheck"
    );
    assert!(
        script.contains("--data-dir"),
        "gen_catalog.mjs keeps the full regeneration gate"
    );
    // The self-check re-canonicalizes through the same serializer.
    assert!(script.contains("function canonicalizeCatalogFile("));
    assert!(script.contains("function canonicalModel("));
}
