//! pi `~/.pi/agent/models.json` support: schema, JSONC parsing, and
//! merge/load into runtime models (upstream `model-config.ts` full schema).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::config::settings::Settings;

use super::config_value::{resolve_config_value, resolve_headers_in};
use super::construction::{build_provider_from_models, model, ModelSpec};
use super::settings;
use crate::ai::error::AiResult;
use crate::ai::provider::ProviderRegistry;
use crate::ai::types::{Model, ModelCostRates, OpenAiCompat};

/// Per-million-token rates as written in a models.json `cost` object
/// (upstream `model-config.ts` camelCase shape). All fields are optional so
/// `modelOverrides` can patch individual rate fields
/// (upstream `applyModelOverride` merges cost per field).
#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PiCostRates {
    #[serde(default)]
    pub input: Option<f64>,
    #[serde(default)]
    pub output: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
}

/// A model entry (or override) from pi's `models.json`
/// (upstream `ModelDefinition` subset).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PiModelConfig {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(alias = "baseUrl", default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(alias = "thinkingLevelMap", default)]
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    #[serde(default)]
    pub input: Option<Vec<String>>,
    #[serde(default)]
    pub cost: Option<PiCostRates>,
    #[serde(
        alias = "contextWindow",
        default,
        deserialize_with = "deserialize_token_count"
    )]
    pub context_window: Option<u64>,
    #[serde(
        alias = "maxTokens",
        default,
        deserialize_with = "deserialize_token_count"
    )]
    pub max_tokens: Option<u64>,
    #[serde(alias = "samplingParams", default)]
    pub sampling_params: Option<BTreeMap<String, Value>>,
    /// Sampling overrides selected by the effective thinking level (upstream
    /// `Model.samplingParamsByThinkingLevel`; routed onto the model's compat
    /// block, where pir carries the field).
    #[serde(alias = "samplingParamsByThinkingLevel", default)]
    pub sampling_params_by_thinking_level: Option<BTreeMap<String, BTreeMap<String, Value>>>,
    /// Input size limits per input type (upstream `Model.inputLimits`).
    #[serde(alias = "inputLimits", default)]
    pub input_limits: Option<Value>,
    /// Prompt-cache configuration (upstream `Model.promptCache`).
    #[serde(alias = "promptCache", default)]
    pub prompt_cache: Option<Value>,
    #[serde(default, deserialize_with = "deserialize_headers_with_null")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub compat: Option<OpenAiCompat>,
}

/// A provider entry from pi's `~/.pi/agent/models.json`
/// (upstream `model-config.ts:168-221`).
#[derive(Clone, Deserialize, Default)]
pub struct PiProviderConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(alias = "baseUrl", default)]
    pub base_url: Option<String>,
    /// API key template: `$ENV` / `${ENV}` / `!command` / literal (resolved
    /// through [`super::config_value`]).
    #[serde(alias = "apiKey", default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    /// Promotion marker for radius OAuth provisioning; only the literal
    /// `"radius"` is meaningful (upstream `oauth: Type.Literal("radius")`).
    #[serde(default)]
    pub oauth: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Extra request headers; values may use `$ENV` / `!command` templates
    /// resolved at load time (upstream `resolveHeaders`). A JSON `null` value
    /// suppresses the same-named default/provider header (stored as the
    /// empty-string suppression marker).
    #[serde(default, deserialize_with = "deserialize_headers_with_null")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub compat: Option<OpenAiCompat>,
    /// Send the credential in the Authorization header (upstream
    /// `authHeader`).
    #[serde(alias = "authHeader", default)]
    pub auth_header: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_models")]
    pub models: Vec<PiModelConfig>,
    /// Per-model overrides applied LAST (after custom-model upserts), to chat
    /// models only (upstream `provider-composer.ts`).
    #[serde(alias = "modelOverrides", default)]
    pub model_overrides: BTreeMap<String, PiModelConfig>,
}

/// Deserialize a header map whose values may be `null` (upstream
/// `ProviderHeaders = Record<string, string | null>`). A `null` value becomes
/// the empty-string suppression marker: it shadows a default/provider header
/// with the same name and is dropped before the request is sent.
fn deserialize_headers_with_null<'de, D>(
    deserializer: D,
) -> Result<Option<BTreeMap<String, String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<BTreeMap<String, Option<String>>>::deserialize(deserializer)?;
    Ok(value.map(|headers| {
        headers
            .into_iter()
            .map(|(name, value)| (name, value.unwrap_or_default()))
            .collect()
    }))
}

/// `contextWindow`/`maxTokens`: pi's schema types both as JSON numbers with
/// `exclusiveMinimum: 0` (a float is schema-valid upstream), while the runtime
/// models store whole tokens. Accept any positive number — a JSON integer
/// keeps its exact value, a fractional one truncates — and reject anything
/// else here as a typed-parse failure (the schema validator above has already
/// reported the pi-shaped error for a wrong type).
fn deserialize_token_count<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(Value::Null) => None,
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|number| *number > 0.0)
            .map(|number| number.trunc() as u64),
        Some(other) => {
            return Err(serde::de::Error::custom(format!(
                "invalid type: {other}, expected a positive number"
            )))
        }
    })
}

fn deserialize_models<'de, D>(deserializer: D) -> Result<Vec<PiModelConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        // An array mixes catalog-reference strings ("acme-large") with full
        // model objects (upstream ModelDefinitionSchema | string).
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::String(id) => Ok(PiModelConfig {
                    id,
                    ..PiModelConfig::default()
                }),
                other => serde_json::from_value(other).map_err(serde::de::Error::custom),
            })
            .collect(),
        Value::String(id) => Ok(vec![PiModelConfig {
            id,
            ..PiModelConfig::default()
        }]),
        Value::Object(map) => map
            .into_iter()
            .map(|(id, value)| {
                let mut model: PiModelConfig =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                model.id = if model.id.is_empty() { id } else { model.id };
                Ok(model)
            })
            .collect(),
        Value::Null => Ok(Vec::new()),
        _ => Err(serde::de::Error::custom(
            "models must be an array, object, or string",
        )),
    }
}

impl std::fmt::Debug for PiModelsConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PiModelsConfig")
            .field("default_provider", &self.default_provider)
            .field("default_model", &self.default_model)
            .field("providers", &self.providers)
            .finish()
    }
}

impl std::fmt::Debug for PiProviderConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PiProviderConfig")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("api", &self.api)
            .field("model", &self.model)
            .field("models", &self.models)
            .field(
                "model_overrides",
                &self.model_overrides.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Pi-compatible provider selection and credentials. Secrets are retained only
/// in memory and are never included in display/status data.
#[derive(Clone, Deserialize, Default)]
pub struct PiModelsConfig {
    #[serde(alias = "defaultProvider", default)]
    pub default_provider: Option<String>,
    #[serde(alias = "defaultModel", default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, PiProviderConfig>,
}

/// Selected runtime model loaded from pi configuration.
#[derive(Clone)]
pub struct PiConfiguredModel {
    pub registry: ProviderRegistry,
    pub model: Model,
    pub api_key: Option<String>,
    /// Per-provider keys resolved from `models.json` (`$ENV` / `${ENV}` /
    /// `!command` / literal), the same map [`PiEnvironment`] exposes. Hosts
    /// that re-resolve the starting model (`--model`) need keys for providers
    /// other than the configured default.
    pub api_keys: BTreeMap<String, String>,
    /// The settings snapshot the selection came from (default thinking
    /// level, theme, editor padding… for the hosts).
    pub settings: Settings,
}

/// Upper bound on the `models.json` source size. [`strip_jsonc`] materializes
/// the whole source as a `Vec<char>` (~4 bytes per ASCII char) plus the input
/// and output strings, so an unbounded read of a hostile file multiplies its
/// size several times over; 8 MiB keeps that peak around 48 MiB.
pub const MAX_PI_MODELS_JSON_BYTES: u64 = 8 * 1024 * 1024;

/// Strip a UTF-8 BOM, `//` and `/* */` comments, and trailing commas so pi's
/// JSONC models.json parses with plain `serde_json` (upstream allows JSONC and
/// strips the BOM).
pub fn strip_jsonc(contents: &str) -> String {
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let chars: Vec<char> = contents.chars().collect();
    let mut out = String::with_capacity(contents.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push('"');
                index += 1;
            }
            '/' if index + 1 < chars.len() && chars[index + 1] == '/' => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            '/' if index + 1 < chars.len() && chars[index + 1] == '*' => {
                index += 2;
                while index + 1 < chars.len() && !(chars[index] == '*' && chars[index + 1] == '/') {
                    index += 1;
                }
                index = (index + 2).min(chars.len());
            }
            ',' => {
                // Trailing comma: skip when only whitespace precedes the
                // closing brace/bracket.
                let mut lookahead = index + 1;
                while lookahead < chars.len()
                    && matches!(chars[lookahead], ' ' | '\t' | '\r' | '\n')
                {
                    lookahead += 1;
                }
                if lookahead < chars.len() && (chars[lookahead] == '}' || chars[lookahead] == ']') {
                    index += 1;
                } else {
                    out.push(',');
                    index += 1;
                }
            }
            _ => {
                out.push(ch);
                index += 1;
            }
        }
    }
    out
}

/// A `models.json` document that pi's `ModelConfig.load` rejects
/// (`packages/coding-agent/src/core/model-config.ts`).
///
/// [`parse_pi_config`] returns this instead of a bare `serde_json::Error` so
/// the two pi error shapes stay distinguishable: a document that does not
/// parse at all (`Failed to parse models.json: <error>`) versus a document
/// that parses but violates pi's schema (`Invalid models.json schema:` with
/// one `  - <path>: <message>` line per violation, PIR-BIN-097).
#[derive(Debug)]
pub enum PiModelsJsonError {
    /// The source is not valid JSON(C).
    Parse(serde_json::Error),
    /// The source parses but violates pi's models.json schema; each entry is
    /// one pi `  - <path>: <message>` line.
    Schema(Vec<String>),
}

impl PiModelsJsonError {
    /// pi's message text without the `File: <path>` trailer
    /// (`ModelConfig.load`).
    pub fn message(&self) -> String {
        match self {
            Self::Parse(error) => format!("Failed to parse models.json: {error}"),
            Self::Schema(errors) => format!("Invalid models.json schema:\n{}", errors.join("\n")),
        }
    }

    /// The schema violations, each already formatted as pi's
    /// `  - <path>: <message>` line (empty for a parse error).
    pub fn schema_error_lines(&self) -> &[String] {
        match self {
            Self::Parse(_) => &[],
            Self::Schema(errors) => errors,
        }
    }
}

impl std::fmt::Display for PiModelsJsonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for PiModelsJsonError {}

/// Parse pi configuration without touching the filesystem. JSONC (comments,
/// trailing commas) and a UTF-8 BOM are tolerated, mirroring upstream
/// (`stripJsonComments(stripBom(content))`); the parsed document is then
/// validated against pi's `ModelsConfigSchema` (PIR-BIN-097: a wrong-typed
/// known field is a reported schema error, never a silently invented model).
pub fn parse_pi_config(contents: &str) -> Result<PiModelsConfig, PiModelsJsonError> {
    let value: Value =
        serde_json::from_str(&strip_jsonc(contents)).map_err(PiModelsJsonError::Parse)?;
    let errors = validate_models_json_schema(&value);
    if !errors.is_empty() {
        return Err(PiModelsJsonError::Schema(errors));
    }
    // Every field pi's schema pins is type-correct at this point; a typed
    // deserialization failure can only come from a corner pir narrows (a
    // wide numeric field, an exotic `compat`/`thinkingLevelMap` value). It
    // still degrades through the parse-error text instead of aborting.
    serde_json::from_value(value).map_err(PiModelsJsonError::Parse)
}

// ---------------------------------------------------------------------------
// pi's models.json schema validation
// ---------------------------------------------------------------------------
//
// `ModelsConfigSchema` (core/model-config.ts plus `pi-ai`'s model-schema.ts)
// as a document walker: pi reports TypeBox violations in schema order with
// `  - <path>: <message>` lines, so the checks below follow the schema field
// order and reproduce the messages the shipped 1.4.0 bundle emits
// (`.play/evidence/p16-modelsjson/runs/schema/`).
//
// Unknown fields are accepted: pi's TypeBox objects allow additional
// properties (schema-probes/unknown-model-field, unknown-provider-field). Two
// pir extensions stay accepted where pi's schema is stricter, both covered by
// existing tests: a `models` array item may be a plain catalog-reference
// string (`ModelDefinitionSchema | string`, tests/suites/ai_provider.rs) and
// a header value may be `null` (the suppression marker,
// tests/ai_lane_g_w17.rs).

/// pi's `ProviderConfigSchema` field order.
const PROVIDER_SCHEMA_FIELDS: &[&str] = &[
    "name",
    "baseUrl",
    "apiKey",
    "api",
    "oauth",
    "headers",
    "compat",
    "authHeader",
    "models",
    "modelOverrides",
];

/// pi's `ModelDefinitionSchema` field order.
const MODEL_SCHEMA_FIELDS: &[&str] = &[
    "id",
    "name",
    "api",
    "baseUrl",
    "reasoning",
    "thinkingLevelMap",
    "input",
    "inputLimits",
    "cost",
    "promptCache",
    "contextWindow",
    "maxTokens",
    "samplingParams",
    "samplingParamsByThinkingLevel",
    "headers",
    "compat",
];

/// pi's `ModelOverrideSchema` field order (the model schema without `id`).
const MODEL_OVERRIDE_SCHEMA_FIELDS: &[&str] = &[
    "name",
    "reasoning",
    "thinkingLevelMap",
    "input",
    "inputLimits",
    "cost",
    "promptCache",
    "contextWindow",
    "maxTokens",
    "samplingParams",
    "samplingParamsByThinkingLevel",
    "headers",
    "compat",
];

/// pi's `MODEL_THINKING_LEVELS`.
const MODEL_THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];
/// pi's `MODEL_INPUT_MODALITIES`.
const MODEL_INPUT_MODALITIES: &[&str] = &["text", "image"];
/// `ModelCostRatesSchema` fields, in required-property order.
const COST_RATE_FIELDS: &[&str] = &["input", "output", "cacheRead", "cacheWrite"];
/// `ModelCostTierSchema` required properties, in schema order.
const COST_TIER_FIELDS: &[&str] = &[
    "inputTokensAbove",
    "input",
    "output",
    "cacheRead",
    "cacheWrite",
];

/// The complete violation list for one parsed document; empty means
/// schema-valid (upstream `validateModelsConfig.Check`). Each entry is already
/// rendered as pi's `  - <path>: <message>` line (the helpers above collect
/// the bare `<path>: <message>` form).
fn validate_models_json_schema(root: &Value) -> Vec<String> {
    let errors = collect_models_json_schema_errors(root);
    errors
        .into_iter()
        .map(|error| format!("  - {error}"))
        .collect()
}

/// The bare `<path>: <message>` violations, in pi's schema order.
fn collect_models_json_schema_errors(root: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    let Value::Object(root) = root else {
        return vec!["root: must be object".to_string()];
    };
    if let Some(schema) = root.get("$schema") {
        check_string("$schema", schema, &mut errors);
    }
    match root.get("providers") {
        None => errors.push("providers: must have required properties providers".to_string()),
        Some(Value::Object(providers)) => {
            for (provider_id, provider) in providers {
                validate_provider(&format!("providers.{provider_id}"), provider, &mut errors);
            }
        }
        Some(_) => errors.push("providers: must be object".to_string()),
    }
    errors
}

/// `Type.String()`.
fn check_string(path: &str, value: &Value, errors: &mut Vec<String>) {
    if !value.is_string() {
        errors.push(format!("{path}: must be string"));
    }
}

/// `Type.String({ minLength: 1 })`.
fn check_non_empty_string(path: &str, value: &Value, errors: &mut Vec<String>) {
    match value {
        Value::String(text) if !text.is_empty() => {}
        Value::String(_) => errors.push(format!("{path}: must not have fewer than 1 characters")),
        _ => errors.push(format!("{path}: must be string")),
    }
}

/// `Type.Boolean()`.
fn check_boolean(path: &str, value: &Value, errors: &mut Vec<String>) {
    if !value.is_boolean() {
        errors.push(format!("{path}: must be boolean"));
    }
}

/// `Type.Object(...)`: the object, or pi's `must be object` line.
fn check_object<'a>(
    path: &str,
    value: &'a Value,
    errors: &mut Vec<String>,
) -> Option<&'a serde_json::Map<String, Value>> {
    match value {
        Value::Object(object) => Some(object),
        _ => {
            errors.push(format!("{path}: must be object"));
            None
        }
    }
}

/// `Type.Number()` (no range constraint).
fn check_number(path: &str, value: &Value, errors: &mut Vec<String>) {
    if !value.is_number() {
        errors.push(format!("{path}: must be number"));
    }
}

/// `Type.Number({ exclusiveMinimum: 0 })`.
fn check_positive_number(path: &str, value: &Value, errors: &mut Vec<String>) {
    match value.as_f64() {
        Some(number) if number > 0.0 => {}
        Some(_) => errors.push(format!("{path}: must be > 0")),
        None => errors.push(format!("{path}: must be number")),
    }
}

/// `Type.Integer({ minimum, maximum })`.
fn check_integer(
    path: &str,
    value: &Value,
    minimum: Option<i64>,
    maximum: Option<i64>,
    errors: &mut Vec<String>,
) {
    match value.as_i64() {
        Some(number) => {
            if let Some(minimum) = minimum {
                if number < minimum {
                    errors.push(format!("{path}: must be >= {minimum}"));
                }
            }
            if let Some(maximum) = maximum {
                if number > maximum {
                    errors.push(format!("{path}: must be <= {maximum}"));
                }
            }
        }
        // A float, an out-of-range integer, or a wrong type all fail the
        // `integer` keyword (TypeBox's message is the keyword's).
        None => errors.push(format!("{path}: must be integer")),
    }
}

fn validate_provider(path: &str, value: &Value, errors: &mut Vec<String>) {
    let Some(object) = check_object(path, value, errors) else {
        return;
    };
    for field in PROVIDER_SCHEMA_FIELDS {
        let Some(field_value) = object.get(*field) else {
            continue;
        };
        let field_path = format!("{path}.{field}");
        match *field {
            "name" | "baseUrl" | "apiKey" | "api" => {
                check_non_empty_string(&field_path, field_value, errors)
            }
            "oauth" => {
                // `Type.Literal("radius")`: a non-string reports BOTH the
                // string type and the literal (schema-probes/oauth-number).
                if !field_value.is_string() {
                    errors.push(format!("{field_path}: must be string"));
                }
                if field_value.as_str() != Some("radius") {
                    errors.push(format!("{field_path}: must be equal to constant"));
                }
            }
            "headers" => validate_headers(&field_path, field_value, errors),
            "compat" => {
                let _ = check_object(&field_path, field_value, errors);
            }
            "authHeader" => check_boolean(&field_path, field_value, errors),
            "models" => match field_value {
                Value::Array(items) => {
                    for (index, item) in items.iter().enumerate() {
                        // pir extension: a plain string references a catalog
                        // model (see the section comment).
                        if item.is_string() {
                            continue;
                        }
                        validate_model(&format!("{field_path}.{index}"), item, false, errors);
                    }
                }
                _ => errors.push(format!("{field_path}: must be array")),
            },
            "modelOverrides" => match field_value {
                Value::Object(overrides) => {
                    for (id, over) in overrides {
                        validate_model(&format!("{field_path}.{id}"), over, true, errors);
                    }
                }
                _ => errors.push(format!("{field_path}: must be object")),
            },
            _ => {}
        }
    }
}

/// One `models` entry (`override_entry = false`) or `modelOverrides` entry
/// (`true`): the schemas differ only in `id` (required for definitions) and a
/// partial `cost` for overrides.
fn validate_model(path: &str, value: &Value, override_entry: bool, errors: &mut Vec<String>) {
    let Some(object) = check_object(path, value, errors) else {
        return;
    };
    if !override_entry && !object.contains_key("id") {
        errors.push(format!("{path}.id: must have required properties id"));
    }
    let fields = if override_entry {
        MODEL_OVERRIDE_SCHEMA_FIELDS
    } else {
        MODEL_SCHEMA_FIELDS
    };
    for field in fields {
        let Some(field_value) = object.get(*field) else {
            continue;
        };
        let field_path = format!("{path}.{field}");
        match *field {
            "id" | "name" | "api" | "baseUrl" => {
                check_non_empty_string(&field_path, field_value, errors)
            }
            "reasoning" => check_boolean(&field_path, field_value, errors),
            "thinkingLevelMap" => validate_thinking_level_map(&field_path, field_value, errors),
            "input" => validate_input_modalities(&field_path, field_value, errors),
            "inputLimits" => validate_input_limits(&field_path, field_value, errors),
            "cost" => validate_cost(&field_path, field_value, override_entry, errors),
            "promptCache" => validate_prompt_cache(&field_path, field_value, errors),
            "contextWindow" | "maxTokens" => {
                check_positive_number(&field_path, field_value, errors)
            }
            "samplingParams" => {
                let _ = check_object(&field_path, field_value, errors);
            }
            "samplingParamsByThinkingLevel" => {
                if let Some(map) = check_object(&field_path, field_value, errors) {
                    for level in MODEL_THINKING_LEVELS {
                        if let Some(entry) = map.get(*level) {
                            let _ = check_object(&format!("{field_path}.{level}"), entry, errors);
                        }
                    }
                }
            }
            "headers" => validate_headers(&field_path, field_value, errors),
            "compat" => {
                let _ = check_object(&field_path, field_value, errors);
            }
            _ => {}
        }
    }
}

/// `HeadersSchema = Record<string, string>`; pir additionally accepts `null`
/// as the header-suppression marker (tests/ai_lane_g_w17.rs), which pi's
/// TypeBox schema rejects (schema-probes/model-header-null).
fn validate_headers(path: &str, value: &Value, errors: &mut Vec<String>) {
    let Some(headers) = check_object(path, value, errors) else {
        return;
    };
    for (name, header) in headers {
        if !header.is_string() && !header.is_null() {
            errors.push(format!("{path}.{name}: must be string"));
        }
    }
}

/// `ThinkingLevelMapSchema`: `Partial<Record<ModelThinkingLevel, string|null>>`.
/// A bad value reports the union exactly like TypeBox
/// (schema-probes/thinking-map-bad-value); unknown level keys are accepted
/// (schema-probes/thinking-level-map-bad-key).
fn validate_thinking_level_map(path: &str, value: &Value, errors: &mut Vec<String>) {
    let Some(map) = check_object(path, value, errors) else {
        return;
    };
    for level in MODEL_THINKING_LEVELS {
        let Some(entry) = map.get(*level) else {
            continue;
        };
        if !entry.is_string() && !entry.is_null() {
            let entry_path = format!("{path}.{level}");
            errors.push(format!("{entry_path}: must be string"));
            errors.push(format!("{entry_path}: must be null"));
            errors.push(format!("{entry_path}: must match a schema in anyOf"));
        }
    }
}

/// `input: Array<ModelInputModality>`.
fn validate_input_modalities(path: &str, value: &Value, errors: &mut Vec<String>) {
    let Value::Array(items) = value else {
        errors.push(format!("{path}: must be array"));
        return;
    };
    for (index, item) in items.iter().enumerate() {
        if !item
            .as_str()
            .is_some_and(|modality| MODEL_INPUT_MODALITIES.contains(&modality))
        {
            errors.push(format!(
                "{path}.{index}: must be equal to one of the allowed values"
            ));
        }
    }
}

/// `ModelInputLimitsSchema`.
fn validate_input_limits(path: &str, value: &Value, errors: &mut Vec<String>) {
    let Some(limits) = check_object(path, value, errors) else {
        return;
    };
    if let Some(max_request_bytes) = limits.get("maxRequestBytes") {
        check_integer(
            &format!("{path}.maxRequestBytes"),
            max_request_bytes,
            Some(1),
            None,
            errors,
        );
    }
    let Some(images) = limits.get("images") else {
        return;
    };
    let images_path = format!("{path}.images");
    let Some(images) = check_object(&images_path, images, errors) else {
        return;
    };
    for field in ["maxPerMessage", "maxPerRequest"] {
        if let Some(limit) = images.get(field) {
            check_integer(
                &format!("{images_path}.{field}"),
                limit,
                Some(1),
                None,
                errors,
            );
        }
    }
    let Some(resize) = images.get("resize") else {
        return;
    };
    let resize_path = format!("{images_path}.resize");
    let Some(resize) = check_object(&resize_path, resize, errors) else {
        return;
    };
    for field in ["maxWidth", "maxHeight", "maxBytes"] {
        if let Some(limit) = resize.get(field) {
            check_integer(
                &format!("{resize_path}.{field}"),
                limit,
                Some(1),
                None,
                errors,
            );
        }
    }
    if let Some(quality) = resize.get("jpegQuality") {
        check_integer(
            &format!("{resize_path}.jpegQuality"),
            quality,
            Some(1),
            Some(100),
            errors,
        );
    }
}

/// `ModelCostSchema` (`partial` = the override schema, where every field is
/// optional). A missing required field reports pi's single line naming the
/// first missing property in the path and all of them in the message
/// (schema-probes/model-cost-partial).
fn validate_cost(path: &str, value: &Value, partial: bool, errors: &mut Vec<String>) {
    let Some(cost) = check_object(path, value, errors) else {
        return;
    };
    if !partial {
        let missing: Vec<&str> = COST_RATE_FIELDS
            .iter()
            .copied()
            .filter(|field| !cost.contains_key(*field))
            .collect();
        if !missing.is_empty() {
            errors.push(format!(
                "{path}.{}: must have required properties {}",
                missing[0],
                missing.join(", ")
            ));
        }
    }
    for field in COST_RATE_FIELDS {
        if let Some(rate) = cost.get(*field) {
            check_number(&format!("{path}.{field}"), rate, errors);
        }
    }
    let Some(tiers) = cost.get("tiers") else {
        return;
    };
    let Value::Array(items) = tiers else {
        errors.push(format!("{path}.tiers: must be array"));
        return;
    };
    for (index, tier) in items.iter().enumerate() {
        let tier_path = format!("{path}.tiers.{index}");
        let Some(tier) = check_object(&tier_path, tier, errors) else {
            continue;
        };
        let missing: Vec<&str> = COST_TIER_FIELDS
            .iter()
            .copied()
            .filter(|field| !tier.contains_key(*field))
            .collect();
        if !missing.is_empty() {
            errors.push(format!(
                "{tier_path}.{}: must have required properties {}",
                missing[0],
                missing.join(", ")
            ));
        }
        for field in COST_TIER_FIELDS {
            if let Some(rate) = tier.get(*field) {
                check_number(&format!("{tier_path}.{field}"), rate, errors);
            }
        }
    }
}

/// `ModelPromptCacheSchema`: `Partial<Record<"short"|"long", number > 0>>`;
/// unknown keys are accepted (schema-probes/prompt-cache-bad).
fn validate_prompt_cache(path: &str, value: &Value, errors: &mut Vec<String>) {
    let Some(cache) = check_object(path, value, errors) else {
        return;
    };
    for tier in ["short", "long"] {
        if let Some(seconds) = cache.get(tier) {
            check_positive_number(&format!("{path}.{tier}"), seconds, errors);
        }
    }
}

/// Apply one models.json model OVERRIDE onto a runtime model (upstream
/// `applyModelOverride`): `cost` merges per field, `thinkingLevelMap` merges
/// per key; everything else replaces.
fn apply_pi_model_override(target: &mut Model, over: &PiModelConfig) {
    if let Some(name) = &over.name {
        target.name = name.clone();
    }
    if let Some(base_url) = &over.base_url {
        target.base_url = base_url.clone();
    }
    if let Some(api) = &over.api {
        target.api = api.clone();
    }
    if let Some(reasoning) = over.reasoning {
        target.reasoning = reasoning;
    }
    if let Some(map) = &over.thinking_level_map {
        let mut merged = target.thinking_level_map.take().unwrap_or_default();
        for (key, value) in map {
            merged.insert(key.clone(), value.clone());
        }
        target.thinking_level_map = Some(merged);
    }
    if let Some(input) = &over.input {
        target.input = input.clone();
    }
    if let Some(rates) = over.cost {
        let existing = target.cost.rates;
        target.cost.rates = ModelCostRates {
            input: rates.input.unwrap_or(existing.input),
            output: rates.output.unwrap_or(existing.output),
            cache_read: rates.cache_read.unwrap_or(existing.cache_read),
            cache_write: rates.cache_write.unwrap_or(existing.cache_write),
        };
    }
    apply_shared_entry_fields(target, over);
}

/// Fields shared by entries and overrides.
fn apply_shared_entry_fields(target: &mut Model, over: &PiModelConfig) {
    if let Some(context_window) = over.context_window {
        target.context_window = context_window;
    }
    if let Some(max_tokens) = over.max_tokens {
        target.max_tokens = max_tokens;
    }
    if let Some(sampling_params) = &over.sampling_params {
        target.sampling_params = Some(sampling_params.clone());
    }
    if let Some(headers) = &over.headers {
        target.headers = Some(headers.clone());
    }
    // Metadata fields pir carries on the compat block (upstream keeps them on
    // the Model): `samplingParamsByThinkingLevel` merges per level for
    // overrides (upstream `mergeSamplingParamsByThinkingLevel`), the rest
    // replace. Only touched when something is set — an entry without compat
    // fields must keep `compat: None` so URL-based auto-detection applies.
    let has_compat_metadata = over.sampling_params_by_thinking_level.is_some()
        || over.input_limits.is_some()
        || over.prompt_cache.is_some();
    if has_compat_metadata || over.compat.is_some() {
        let compat = target.compat.get_or_insert_with(OpenAiCompat::default);
        if let Some(map) = &over.sampling_params_by_thinking_level {
            // Upstream `mergeSamplingParamsByThinkingLevel` merges WITHIN a
            // level: the override's params are added onto the entry's params
            // for that level, not replacing the whole level entry.
            let mut merged = compat
                .sampling_params_by_thinking_level
                .take()
                .unwrap_or_default();
            for (level, params) in map {
                let level_params = merged.entry(level.clone()).or_default();
                for (name, value) in params {
                    level_params.insert(name.clone(), value.clone());
                }
            }
            compat.sampling_params_by_thinking_level = Some(merged);
        }
        if let Some(input_limits) = &over.input_limits {
            compat.input_limits = Some(input_limits.clone());
        }
        if let Some(prompt_cache) = &over.prompt_cache {
            compat.prompt_cache = Some(prompt_cache.clone());
        }
        if let Some(compat_override) = &over.compat {
            // `compat` replaces (upstream `mergeCompat` at the models.json
            // entry boundary); the metadata fields already applied above are
            // carried over so a compat block in the entry does not drop them.
            let mut merged_compat = compat_override.clone();
            merged_compat.input_limits = compat.input_limits.take();
            merged_compat.prompt_cache = compat.prompt_cache.take();
            if merged_compat.sampling_params_by_thinking_level.is_none() {
                merged_compat.sampling_params_by_thinking_level =
                    compat.sampling_params_by_thinking_level.take();
            }
            target.compat = Some(merged_compat);
        } else {
            target.compat = Some(compat.clone());
        }
    }
}

/// Build a fully-populated runtime model from a models.json model entry,
/// seeded with the upstream defaults (`findModelDefaults`): an existing
/// model's api/baseUrl when neither the entry nor the provider config
/// specifies them.
fn pi_model_from_config(
    provider_id: &str,
    entry: &PiModelConfig,
    provider_api: Option<&str>,
    provider_base_url: Option<&str>,
    defaults: Option<&Model>,
) -> Model {
    let api = entry
        .api
        .clone()
        .or_else(|| provider_api.map(str::to_string))
        .or_else(|| defaults.map(|model| model.api.clone()))
        .unwrap_or_else(|| "openai-chat".to_string());
    let base_url = entry
        .base_url
        .clone()
        .or_else(|| provider_base_url.map(str::to_string))
        .or_else(|| defaults.map(|model| model.base_url.clone()))
        .unwrap_or_else(|| "http://localhost:11434/v1".to_string());
    let mut target = model(ModelSpec {
        provider: provider_id,
        id: &entry.id,
        name: entry.id.as_str(),
        api: &api,
        base_url: &base_url,
        context_window: 128_000,
        max_tokens: 16_384,
        input: vec!["text"],
    });
    target.id = entry.id.clone();
    target.provider = provider_id.to_string();
    if let Some(name) = &entry.name {
        target.name = name.clone();
    }
    if let Some(reasoning) = entry.reasoning {
        target.reasoning = reasoning;
    }
    if let Some(map) = &entry.thinking_level_map {
        target.thinking_level_map = Some(map.clone());
    }
    if let Some(input) = &entry.input {
        target.input = input.clone();
    }
    if let Some(rates) = entry.cost {
        target.cost.rates = ModelCostRates {
            input: rates.input.unwrap_or(0.0),
            output: rates.output.unwrap_or(0.0),
            cache_read: rates.cache_read.unwrap_or(0.0),
            cache_write: rates.cache_write.unwrap_or(0.0),
        };
    }
    apply_shared_entry_fields(&mut target, entry);
    // Keep the routed catalog model's static headers (upstream layering: the
    // entry overlays the existing model, so unset headers survive); explicit
    // entry headers (including null-suppression markers) win per name.
    if let Some(default_headers) = defaults
        .filter(|model| model.id == entry.id)
        .and_then(|model| model.headers.as_ref())
    {
        let mut merged = default_headers.clone();
        if let Some(entry_headers) = target.headers.take() {
            for (name, value) in entry_headers {
                crate::ai::types::insert_header(&mut merged, &name, value);
            }
        }
        target.headers = Some(merged);
    }
    target
}

/// The upstream `findModelDefaults` fallback chain for an entry without
/// api/baseUrl: a model with the same id, else one with the requested api,
/// else one with the `openai-completions` api, else the first model.
fn find_model_defaults<'a>(
    models: &'a [Model],
    model_id: &str,
    api: Option<&str>,
) -> Option<&'a Model> {
    models
        .iter()
        .find(|model| model.id == model_id)
        .or_else(|| api.and_then(|api| models.iter().find(|model| model.api == api)))
        .or_else(|| {
            models
                .iter()
                .find(|model| model.api == "openai-completions")
        })
        .or_else(|| models.first())
}

/// Compose one provider's model list from the built-in catalog plus its
/// models.json entry (upstream `applyModelsJson` + the `modelOverrides` pass
/// of `getAllModels`):
///
/// 1. start from the built-in provider's models (provider-level
///    `baseUrl`/`compat` applied), so config entries LAYER on the catalog
///    instead of replacing it;
/// 2. upsert `models` entries onto that list by id;
/// 3. apply `modelOverrides` to the merged list;
/// 4. merge the provider-level `headers` under every model's headers.
type ComposedProviderModels = (Vec<Model>, Option<BTreeMap<String, String>>);

fn compose_provider_models(
    provider_id: &str,
    provider_config: &PiProviderConfig,
    builtin: Option<&super::catalog::BuiltinProvider>,
    agent_dir: &Path,
) -> AiResult<ComposedProviderModels> {
    let provider_compat = provider_config.compat.clone();
    let provider_base_url = provider_config.base_url.clone();
    let provider_api = provider_config.api.as_deref();
    let radius_oauth = provider_config.oauth.as_deref() == Some("radius");

    // Header values may use `$ENV` / `${ENV}` / `!command` templates;
    // unresolved headers are dropped (upstream `resolveHeaders`).
    let provider_headers = resolve_headers_in(agent_dir, provider_config.headers.as_ref(), None);
    // `authHeader: true` sends the resolved api key as `Authorization: Bearer`
    // (upstream `withConfiguredAuth`); pir stamps it onto each model's compat
    // so the registry stream path applies it at request time.
    let auth_header = provider_config.auth_header;

    // 1. Built-in catalog models as the base layer.
    let mut models: Vec<Model> = match builtin {
        Some(builtin) => builtin
            .models
            .iter()
            .map(|model| {
                let mut model = model.clone();
                // The provider-level baseUrl replaces each model's, except
                // radius OAuth keeps the catalog endpoint.
                if !radius_oauth {
                    if let Some(base_url) = &provider_base_url {
                        model.base_url = base_url.clone();
                    }
                }
                if let Some(compat) = &provider_compat {
                    model.compat = Some(compat.merged_with(model.compat.as_ref()));
                }
                if auth_header == Some(true) {
                    model
                        .compat
                        .get_or_insert_with(OpenAiCompat::default)
                        .auth_header = Some(true);
                }
                model
            })
            .collect(),
        None => Vec::new(),
    };

    // 2. Upsert entries (existing models replaced by id, new ones appended).
    for entry in &provider_config.models {
        if entry.id.is_empty() {
            continue;
        }
        let defaults =
            find_model_defaults(&models, &entry.id, entry.api.as_deref().or(provider_api));
        let mut model = pi_model_from_config(
            provider_id,
            entry,
            provider_api,
            provider_base_url.as_deref().filter(|_| !radius_oauth),
            defaults,
        );
        if let Some(compat) = &provider_compat {
            model.compat = Some(compat.merged_with(model.compat.as_ref()));
        }
        if auth_header == Some(true) {
            model
                .compat
                .get_or_insert_with(OpenAiCompat::default)
                .auth_header = Some(true);
        }
        match models.iter_mut().find(|m| m.id == model.id) {
            Some(slot) => *slot = model,
            None => models.push(model),
        }
    }

    // 3. modelOverrides patch the merged list by id.
    if !provider_config.model_overrides.is_empty() {
        for model in &mut models {
            if let Some(over) = provider_config.model_overrides.get(&model.id) {
                apply_pi_model_override(model, over);
                // The override's compat block replaced the stamped one.
                if auth_header == Some(true) {
                    model
                        .compat
                        .get_or_insert_with(OpenAiCompat::default)
                        .auth_header = Some(true);
                }
            }
        }
    }

    // 4. Provider-level headers apply to EVERY model of the provider — the
    // built-in catalog entries, the models.json entries and the
    // override-patched ones alike — with the model's own headers winning per
    // name. Applying them only while building configured entries dropped them
    // entirely for a config that sets `headers` without listing `models`
    // (the built-in catalog models never saw them).
    if let Some(provider_headers) = &provider_headers {
        for model in &mut models {
            apply_provider_headers(model, provider_headers);
        }
    }

    Ok((models, provider_headers))
}

/// Merge provider-level `headers` UNDER a model's own headers (the model's
/// catalog/entry/override header wins per name, case-insensitively).
fn apply_provider_headers(model: &mut Model, provider_headers: &BTreeMap<String, String>) {
    let mut merged = provider_headers.clone();
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            crate::ai::types::insert_header(&mut merged, name, value.clone());
        }
    }
    model.headers = Some(merged);
}

/// Build runtime models from pi's `models.json` (upstream `applyModelsJson`):
/// per provider, the built-in catalog models are the base layer, entries in
/// `models` upsert onto it, `modelOverrides` patch the merged list by id, and
/// provider-level `baseUrl`/`api`/`compat`/`headers` apply to every model of
/// that provider.
pub fn merge_pi_config(config: &PiModelsConfig) -> AiResult<Vec<Model>> {
    merge_pi_config_in(&settings::agent_dir(), config)
}

/// [`merge_pi_config`] with an EXPLICIT agent dir: `!command` header values
/// resolve against that dir's config sources instead of the process
/// environment's, so an SDK agent bound to an explicit dir never sends a
/// header cached from another dir.
pub fn merge_pi_config_in(agent_dir: &Path, config: &PiModelsConfig) -> AiResult<Vec<Model>> {
    let mut merged: Vec<Model> = Vec::new();
    for (provider_id, provider_config) in &config.providers {
        let builtin = super::catalog::load_provider_cached(provider_id)?;
        let (models, _provider_headers) =
            compose_provider_models(provider_id, provider_config, builtin.as_deref(), agent_dir)?;
        merged.extend(models);
    }
    Ok(merged)
}

/// Load `~/.pi/agent/models.json` + `settings.json` and select the
/// configured default model.
///
/// Model selection mirrors upstream `findInitialModel`: the
/// `defaultProvider`/`defaultModel` pair lives in **settings.json** (with
/// `models.json`-level `defaultProvider` kept as a legacy fallback). Missing
/// files return `None`; malformed files are reported. The project settings
/// scope loads only when the project is trusted.
pub fn load_pi_config() -> AiResult<Option<PiConfiguredModel>> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    load_pi_config_with(Some(&cwd))
}

/// [`load_pi_config`] with an explicit project directory (for the project
/// settings scope and the trust check). Also used by tests.
///
/// Trust comes from the same resolved order as the run gate
/// ([`crate::extensions::project_trust::resolve_project_trust`]): the pinned
/// startup decision first, so an extension grant/denial or an explicit
/// `--approve`/`--no-approve` that the host resolved at startup is still in
/// effect for `models list` / RPC / TUI config resolution; without a
/// pinned/extension/override decision the trust store /
/// `defaultProjectTrust` policy decides exactly as before.
///
/// A malformed or unreadable settings scope is NOT an error here: the
/// manager keeps that scope's defaults and records a diagnostic (see
/// [`settings::load_settings`]), so a config typo never turns into an
/// `authentication failure` that blocks a run (PIR-BIN-071).
pub fn load_pi_config_with(project_dir: Option<&Path>) -> AiResult<Option<PiConfiguredModel>> {
    let global_dir = settings::agent_dir();
    let project_trusted = match project_dir {
        Some(cwd) => crate::extensions::project_trust::resolve_project_trust(cwd, &global_dir),
        None => false,
    };
    load_pi_config_from(settings::load_settings(project_dir, project_trusted))
}

/// [`load_pi_config_with`] on an ALREADY-loaded [`SettingsManager`] snapshot:
/// the CLI loads settings once at startup and hands the snapshot to the mode
/// hosts, so the TUI does not re-read both scopes here. The manager's typed
/// settings are tolerant (wrong-typed fields fall back to defaults and were
/// reported through [`SettingsManager::drain_errors`] by the CLI).
///
/// [`SettingsManager`]: crate::config::settings::SettingsManager
pub fn load_pi_config_with_settings(
    manager: &crate::config::settings::SettingsManager,
) -> AiResult<Option<PiConfiguredModel>> {
    load_pi_config_from(manager.settings.clone())
}

/// The shared body of the loader variants: `models.json` + a resolved
/// [`Settings`] snapshot.
///
/// A malformed `models.json` is NOT an error: the layer becomes empty (pi's
/// `ModelConfig.load`), the built-in catalog stays available, and the failure
/// is recorded for the CLI surfaces (`take_models_json_load_warning`, pi's
/// `Warning: errors loading models.json:`) — PIR-BIN-098, mirroring the
/// settings recovery of PIR-BIN-071.
fn load_pi_config_from(mut pi_settings: Settings) -> AiResult<Option<PiConfiguredModel>> {
    // models.json is optional: the Claude Code environment can configure the
    // anthropic provider on its own.
    let config = match read_pi_models_json() {
        Some(contents) => {
            let (config, error) = parse_models_json_layer(&contents);
            record_models_json_load_error(error);
            config
        }
        None => {
            record_models_json_load_error(None);
            PiModelsConfig::default()
        }
    };
    let mut registry = auth_backed_registry();

    // Per-provider keys resolved from models.json templates. Stored alongside
    // the selected model so hosts resolving a different `--model` can apply
    // that provider's credential instead of only the configured default's.
    let api_keys: BTreeMap<String, String> = config
        .providers
        .iter()
        .filter_map(|(provider_id, provider_config)| {
            provider_config
                .api_key
                .as_deref()
                .and_then(|key| resolve_config_value(key, None))
                .map(|key| (provider_id.clone(), key))
        })
        .collect();

    // Per-provider composed lists: built-in catalog seeded, entries upserted,
    // overrides applied (upstream `applyModelsJson`).
    let mut composed: Vec<(String, Vec<Model>)> = Vec::new();
    let agent_dir = settings::agent_dir();
    for (provider_id, provider_config) in &config.providers {
        let builtin = super::catalog::load_provider_cached(provider_id)?;
        let (mut models, provider_headers) =
            compose_provider_models(provider_id, provider_config, builtin.as_deref(), &agent_dir)?;
        if models.is_empty() {
            // Legacy single-model provider entry (e.g. a local Ollama
            // endpoint) with no model list and no built-in entry: synthesize
            // it.
            let base_url = provider_config
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434/v1".to_string());
            let model_id = provider_config
                .model
                .clone()
                .or_else(|| provider_config.models.first().map(|m| m.id.clone()))
                .unwrap_or_else(|| "default".to_string());
            let entry = provider_config
                .models
                .iter()
                .find(|m| m.id == model_id)
                .cloned()
                .unwrap_or(PiModelConfig {
                    id: model_id.clone(),
                    ..PiModelConfig::default()
                });
            let mut synthesized = pi_model_from_config(provider_id, &entry, None, None, None);
            synthesized.base_url = base_url;
            // The provider-level header layer applies to a legacy single-model
            // entry exactly like to a composed catalog model.
            if let Some(provider_headers) = provider_headers.as_ref() {
                apply_provider_headers(&mut synthesized, provider_headers);
            }
            models = vec![synthesized];
        }
        let display_name = provider_config.name.clone().unwrap_or_else(|| {
            builtin
                .as_deref()
                .map(|builtin| builtin.display_name.clone())
                .unwrap_or_else(|| provider_id.clone())
        });
        let provider =
            provider_from_models(provider_id, &display_name, &models, builtin.is_some())?;
        registry.set_provider(provider);
        composed.push((provider_id.clone(), models));
    }

    // Claude Code environment compatibility. Explicit models.json values win;
    // env values only fill gaps (see `apply_claude_code_env`).
    let claude_env = ClaudeCodeEnvConfig::from_process_env();
    if let Some(env) = &claude_env {
        let models = bootstrap_anthropic_env(&mut registry, &config, env)?;
        match composed.iter_mut().find(|(id, _)| id == "anthropic") {
            Some((_, existing)) => *existing = models,
            None => composed.push(("anthropic".to_string(), models)),
        }
        if let Some((provider, model)) = claude_env_default(&pi_settings, &config, env) {
            // Environment default only when neither settings.json nor
            // models.json declared one.
            pi_settings.default_provider = Some(provider);
            pi_settings.default_model = Some(model);
        }
    }

    // Default provider: settings.json first (upstream home of
    // `defaultProvider`), then the legacy models.json-level key, then the
    // first configured provider.
    let provider_id = pi_settings
        .default_provider
        .clone()
        .or(config.default_provider.clone())
        .or_else(|| config.providers.keys().next().cloned())
        .or_else(|| composed.first().map(|(id, _)| id.clone()));
    let Some(provider_id) = provider_id else {
        return Ok(None);
    };
    let provider_config = config.providers.get(&provider_id);
    let empty_models: Vec<Model> = Vec::new();
    let provider_models = composed
        .iter()
        .find(|(id, _)| *id == provider_id)
        .map(|(_, models)| models)
        .unwrap_or(&empty_models);
    if provider_models.is_empty() {
        return Ok(None);
    }
    let env_primary = (provider_id == "anthropic")
        .then(|| {
            claude_env
                .as_ref()
                .and_then(|env| env.primary_model.clone())
        })
        .flatten();
    let model_id = pi_settings
        .default_model
        .clone()
        .or(config.default_model.clone())
        .or_else(|| provider_config.and_then(|config| config.model.clone()))
        .or(env_primary)
        .or_else(|| provider_models.first().map(|model| model.id.clone()))
        .unwrap_or_else(|| "default".to_string());
    let model = provider_models
        .iter()
        .find(|m| m.id == model_id)
        .cloned()
        .unwrap_or_else(|| {
            model(ModelSpec {
                provider: &provider_id,
                id: &model_id,
                name: &model_id,
                api: "openai-chat",
                base_url: "http://localhost:11434/v1",
                context_window: 128_000,
                max_tokens: 16_384,
                input: vec!["text"],
            })
        });
    Ok(Some(PiConfiguredModel {
        registry,
        model,
        // API key templates (`$ENV` / `${ENV}` / `!command` / literal) resolve
        // at selection time (upstream `resolveConfigValue`). Environment-only
        // Claude Code auth (ANTHROPIC_AUTH_TOKEN) is resolved by the adapter.
        api_key: provider_config
            .and_then(|config| config.api_key.as_deref())
            .and_then(|key| resolve_config_value(key, None)),
        api_keys,
        settings: pi_settings,
    }))
}

/// The models.json path inside the agent dir
/// (`crate::ai::providers::settings::agent_dir()`, the single source of truth
/// for `PIR_AGENT_DIR`/`PI_CODING_AGENT_DIR` overrides).
fn pi_models_path() -> PathBuf {
    settings::agent_dir().join("models.json")
}

/// The user-facing text for a rejected `models.json`: pi's wording
/// (`ModelConfig.load`) including the resolved file path, so the message names
/// the file this process actually read — never the default `~/.pi/agent` path
/// under a redirected agent dir.
fn models_json_error_text(error: &PiModelsJsonError) -> String {
    format!(
        "{}\n\nFile: {}",
        error.message(),
        pi_models_path().display()
    )
}

/// [`models_json_error_text`] against an explicit path (the SDK builder owns
/// its agent directory instead of the process agent dir).
pub(crate) fn models_json_error_text_at(path: &Path, error: &PiModelsJsonError) -> String {
    format!("{}\n\nFile: {}", error.message(), path.display())
}

/// pi's `ModelConfig.load` on an already-read source: the parsed layer, or the
/// empty layer plus pi's error text. Never an `Err`: a models.json typo
/// degrades to the built-in catalog exactly like a settings.json typo
/// (PIR-BIN-071) instead of aborting every run (PIR-BIN-098).
fn parse_models_json_layer(contents: &str) -> (PiModelsConfig, Option<String>) {
    match parse_pi_config(contents) {
        Ok(config) => (config, None),
        Err(error) => (
            PiModelsConfig::default(),
            Some(models_json_error_text(&error)),
        ),
    }
}

/// [`parse_models_json_layer`] with an explicit `File:` path (the SDK loader,
/// which owns its agent directory rather than the process agent dir).
pub fn parse_models_json_layer_at(path: &Path, contents: &str) -> (PiModelsConfig, Option<String>) {
    match parse_pi_config(contents) {
        Ok(config) => (config, None),
        Err(error) => (
            PiModelsConfig::default(),
            Some(models_json_error_text_at(path, &error)),
        ),
    }
}

/// The failure of the most recent `models.json` load in this process, pi's
/// `ModelConfig.getError()`. Loaders record it on every call (clearing it when
/// the current source is fine); CLI surfaces read it through
/// [`take_models_json_load_warning`] so a config typo is reported instead of
/// silently dropping the file's providers.
static MODELS_JSON_LOAD_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Monotonic count of `models.json` load failures in this process (PIR-BIN-098
/// made the loaders degrade to the built-in layer instead of returning an
/// error, so a caller can no longer use `Err` to tell "degraded" from "fine").
/// A caller that caches a derived view samples this around its load: the
/// session's credential environment MUST NOT cache a degraded load — the file
/// can be fixed while the run continues, and the next push has to retry.
static MODELS_JSON_LOAD_FAILURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many `models.json` load failures this process has recorded (see
/// [`MODELS_JSON_LOAD_FAILURES`]).
pub fn models_json_load_failures() -> u64 {
    MODELS_JSON_LOAD_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record (or clear) the latest `models.json` load failure. Public for hosts
/// that load an explicit agent directory through
/// [`parse_models_json_layer_at`] (the SDK builder).
pub fn record_models_json_load_error(error: Option<String>) {
    if error.is_some() {
        MODELS_JSON_LOAD_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    *MODELS_JSON_LOAD_ERROR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = error;
}

/// The `models.json` error the most recent loader call recorded, if any
/// (non-draining; [`take_models_json_load_error`] consumes it).
pub fn models_json_load_error() -> Option<String> {
    MODELS_JSON_LOAD_ERROR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// [`models_json_load_error`] and clear it: a per-process CLI surface reports
/// the warning once.
pub fn take_models_json_load_error() -> Option<String> {
    MODELS_JSON_LOAD_ERROR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

/// The complete stderr line pi prints when a models.json load failed
/// (`cli/list-models.ts`): `Warning: errors loading models.json:\n<error>`.
/// Draining: the caller reports it once.
pub fn take_models_json_load_warning() -> Option<String> {
    take_models_json_load_error()
        .map(|error| format!("Warning: errors loading models.json:\n{error}"))
}

/// Read the `models.json` source from [`pi_models_path`], treating a file
/// above [`MAX_PI_MODELS_JSON_BYTES`] as absent (and diagnosing it once per
/// load).
fn read_pi_models_json() -> Option<String> {
    read_pi_models_json_from(&pi_models_path())
}

/// [`read_pi_models_json`] for an explicit path. `None` means "no usable
/// file": missing, unreadable, or above the size cap.
fn read_pi_models_json_from(path: &Path) -> Option<String> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_PI_MODELS_JSON_BYTES => {
            crate::ext_diag!("[pi-config] models.json exceeds 8MiB; ignoring");
            None
        }
        _ => std::fs::read_to_string(path).ok(),
    }
}

/// A provider registry backed by the session agent dir's `auth.json`, so a
/// credential written by `/login` (or the auth CLI) resolves for every request
/// instead of only for status queries (`has_configured_auth`).
fn auth_backed_registry() -> ProviderRegistry {
    let agent_dir = settings::agent_dir();
    ProviderRegistry::default().with_auth_storage(std::sync::Arc::new(
        crate::ai::auth::FileAuthStorage::in_agent_dir(&agent_dir),
    ))
}

/// Build the runtime provider for a composed model list: catalog-backed
/// providers group by api id (mixed adapters), unknown providers use the
/// first model's api.
fn provider_from_models(
    provider_id: &str,
    display_name: &str,
    models: &[Model],
    catalog_backed: bool,
) -> AiResult<crate::ai::provider::ProviderBox> {
    if catalog_backed {
        super::construction::CatalogProviderEntry::from_models(provider_id, display_name, models)
    } else {
        build_provider_from_models(provider_id, display_name, models)
    }
}

/// [`provider_from_models`] for a caller that already owns the composed list
/// and keeps no copy of it: the catalog-backed wrapper takes the list by
/// value, so those models are not copied a second time on the way into the
/// provider (the composed list is the provider's own field afterwards).
fn provider_from_owned_models(
    provider_id: &str,
    display_name: &str,
    models: Vec<Model>,
    catalog_backed: bool,
) -> AiResult<crate::ai::provider::ProviderBox> {
    if catalog_backed {
        super::construction::CatalogProviderEntry::from_owned_models(
            provider_id.to_string(),
            display_name.to_string(),
            models,
        )
    } else {
        build_provider_from_models(provider_id, display_name, &models)
    }
}

// ---------------------------------------------------------------------------
// Claude Code environment compatibility
// ---------------------------------------------------------------------------

/// Claude Code's gateway URL (all Anthropic-protocol traffic is redirected to
/// it when models.json does not configure a base URL).
pub const ANTHROPIC_BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";
/// Claude Code's primary model id.
pub const ANTHROPIC_MODEL_ENV: &str = "ANTHROPIC_MODEL";
/// Claude Code's per-alias model ids (Opus / Sonnet / Haiku).
pub const ANTHROPIC_DEFAULT_OPUS_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_OPUS_MODEL";
pub const ANTHROPIC_DEFAULT_SONNET_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_SONNET_MODEL";
pub const ANTHROPIC_DEFAULT_HAIKU_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_HAIKU_MODEL";
/// Claude Code's context-window and max-output overrides.
pub const CLAUDE_CODE_MAX_CONTEXT_TOKENS_ENV: &str = "CLAUDE_CODE_MAX_CONTEXT_TOKENS";
pub const CLAUDE_CODE_MAX_OUTPUT_TOKENS_ENV: &str = "CLAUDE_CODE_MAX_OUTPUT_TOKENS";

/// Defaults for the synthesized env model when the `CLAUDE_CODE_*` overrides
/// are absent (the values upstream Claude Code uses).
const CLAUDE_CODE_DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
const CLAUDE_CODE_DEFAULT_MAX_TOKENS: u64 = 32_000;
/// Last-resort base URL when the built-in catalog has no anthropic entry.
const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The Claude Code environment values that bootstrap the `anthropic` provider.
///
/// Only environment values are used (no model id default is hardcoded, and
/// nothing is written to disk). `None` means the environment carries no
/// Claude Code configuration and the normal models.json/catalog behavior is
/// untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeEnvConfig {
    /// `ANTHROPIC_BASE_URL`, trailing slashes trimmed.
    pub base_url: Option<String>,
    /// Model ids from `ANTHROPIC_MODEL` then the three `ANTHROPIC_DEFAULT_*`
    /// aliases, deduplicated in that order.
    pub model_ids: Vec<String>,
    /// The primary model: `ANTHROPIC_MODEL`, else the first alias id.
    pub primary_model: Option<String>,
    /// `CLAUDE_CODE_MAX_CONTEXT_TOKENS` (default 200000).
    pub context_window: u64,
    /// `CLAUDE_CODE_MAX_OUTPUT_TOKENS` (default 32000).
    pub max_tokens: u64,
}

impl ClaudeCodeEnvConfig {
    /// Read the Claude Code compatibility variables from the process
    /// environment. Empty/whitespace-only values count as unset; malformed or
    /// zero `CLAUDE_CODE_*` values fall back to their defaults.
    pub fn from_process_env() -> Option<Self> {
        let base_url = nonempty_env(ANTHROPIC_BASE_URL_ENV)
            .map(|url| url.trim_end_matches('/').to_string())
            .filter(|url| !url.is_empty());
        let mut model_ids: Vec<String> = Vec::new();
        for name in [
            ANTHROPIC_MODEL_ENV,
            ANTHROPIC_DEFAULT_OPUS_MODEL_ENV,
            ANTHROPIC_DEFAULT_SONNET_MODEL_ENV,
            ANTHROPIC_DEFAULT_HAIKU_MODEL_ENV,
        ] {
            if let Some(id) = nonempty_env(name) {
                if !model_ids.contains(&id) {
                    model_ids.push(id);
                }
            }
        }
        if base_url.is_none() && model_ids.is_empty() {
            return None;
        }
        let primary_model =
            nonempty_env(ANTHROPIC_MODEL_ENV).or_else(|| model_ids.first().cloned());
        Some(Self {
            base_url,
            model_ids,
            primary_model,
            context_window: positive_env_u64(CLAUDE_CODE_MAX_CONTEXT_TOKENS_ENV)
                .unwrap_or(CLAUDE_CODE_DEFAULT_CONTEXT_WINDOW),
            max_tokens: positive_env_u64(CLAUDE_CODE_MAX_OUTPUT_TOKENS_ENV)
                .unwrap_or(CLAUDE_CODE_DEFAULT_MAX_TOKENS),
        })
    }
}

/// Read a process environment variable, treating empty/whitespace-only values
/// as unset.
fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// A positive decimal `u64` environment override; malformed and zero values
/// fall back to the caller's default.
fn positive_env_u64(name: &str) -> Option<u64> {
    nonempty_env(name)?
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
}

/// Apply the Claude Code environment to the anthropic provider's models.
///
/// Precedence (documented in `docs/models.md`):
/// - an explicit models.json provider `baseUrl` wins over `ANTHROPIC_BASE_URL`;
///   otherwise the env URL is applied to every model without an explicit
///   per-model `baseUrl` (an explicit per-model `baseUrl` always wins);
/// - a model id already defined by models.json is never duplicated and its
///   explicit settings (name, context, limits, base URL, headers) win;
///   env ids only add the missing models;
/// - the synthesized entry is reasoning-capable with text+image input,
///   `contextWindow` from `CLAUDE_CODE_MAX_CONTEXT_TOKENS` (default 200000)
///   and `maxTokens` from `CLAUDE_CODE_MAX_OUTPUT_TOKENS` (default 32000).
fn apply_claude_code_env(
    mut models: Vec<Model>,
    provider_config: Option<&PiProviderConfig>,
    builtin: Option<&super::catalog::BuiltinProvider>,
    env: &ClaudeCodeEnvConfig,
) -> Vec<Model> {
    let explicit_provider_base = provider_config.and_then(|config| config.base_url.clone());
    let mut explicit_model_base: Vec<&str> = Vec::new();
    if let Some(config) = provider_config {
        for entry in &config.models {
            if entry.base_url.is_some() {
                explicit_model_base.push(entry.id.as_str());
            }
        }
        for (id, over) in &config.model_overrides {
            if over.base_url.is_some() {
                explicit_model_base.push(id.as_str());
            }
        }
    }

    if let Some(base_url) = env.base_url.as_deref() {
        if explicit_provider_base.is_none() {
            for model in &mut models {
                if !explicit_model_base.contains(&model.id.as_str()) {
                    model.base_url = base_url.to_string();
                }
            }
        }
    }

    let fallback_base = explicit_provider_base
        .or_else(|| env.base_url.clone())
        .or_else(|| models.first().map(|model| model.base_url.clone()))
        .or_else(|| {
            builtin
                .and_then(|builtin| builtin.models.first())
                .map(|model| model.base_url.clone())
        })
        .unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.to_string());
    for id in &env.model_ids {
        if models.iter().any(|model| model.id == *id) {
            continue;
        }
        let mut synthesized = model(ModelSpec {
            provider: "anthropic",
            id,
            name: id,
            api: "anthropic-messages",
            base_url: &fallback_base,
            context_window: env.context_window,
            max_tokens: env.max_tokens,
            input: vec!["text", "image"],
        });
        synthesized.reasoning = true;
        models.push(synthesized);
    }
    models
}

/// Compose the anthropic provider's model list with the Claude Code
/// environment applied and register it (catalog-backed when the built-in
/// catalog has an entry, so the registry keeps the catalog behavior).
fn bootstrap_anthropic_env(
    registry: &mut ProviderRegistry,
    config: &PiModelsConfig,
    env: &ClaudeCodeEnvConfig,
) -> AiResult<Vec<Model>> {
    let builtin = super::catalog::load_provider_cached("anthropic")?;
    let provider_config = config.providers.get("anthropic");
    let existing = match provider_config {
        Some(provider_config) => {
            compose_provider_models(
                "anthropic",
                provider_config,
                builtin.as_deref(),
                &settings::agent_dir(),
            )?
            .0
        }
        None => builtin
            .as_deref()
            .map(|builtin| builtin.models.clone())
            .unwrap_or_default(),
    };
    let models = apply_claude_code_env(existing, provider_config, builtin.as_deref(), env);
    let display_name = provider_config
        .and_then(|config| config.name.clone())
        .or_else(|| {
            builtin
                .as_deref()
                .map(|builtin| builtin.display_name.clone())
        })
        .unwrap_or_else(|| "Anthropic".to_string());
    let provider = provider_from_models("anthropic", &display_name, &models, builtin.is_some())?;
    registry.set_provider(provider);
    Ok(models)
}

/// The env-derived default model, used only when neither settings.json nor
/// models.json declares a default (environment values fill gaps; explicit
/// configuration wins).
fn claude_env_default(
    settings: &Settings,
    config: &PiModelsConfig,
    env: &ClaudeCodeEnvConfig,
) -> Option<(String, String)> {
    let explicit_default = settings.default_provider.is_some()
        || settings.default_model.is_some()
        || config.default_provider.is_some()
        || config.default_model.is_some();
    if explicit_default {
        return None;
    }
    env.primary_model
        .clone()
        .map(|model| ("anthropic".to_string(), model))
}

/// The full model environment for the upstream resolution order: the built-in
/// catalog plus `models.json` providers (models.json wins per provider/model),
/// the per-provider resolved API keys, and the settings snapshot.
pub struct PiEnvironment {
    pub registry: ProviderRegistry,
    /// All available models (catalog + models.json, models.json wins).
    pub models: Vec<Model>,
    /// Resolved API key per provider (from models.json templates).
    pub api_keys: BTreeMap<String, String>,
    pub settings: Settings,
}

/// Load the full model environment (upstream `ModelRuntime.getAvailable`).
/// The project settings scope loads only when the project is trusted.
///
/// Trust comes from the same resolved order as the run gate
/// ([`crate::extensions::project_trust::resolve_project_trust`]): the pinned
/// startup decision first, so an extension grant/denial or an explicit
/// `--approve`/`--no-approve` that the print/json host resolved at startup is
/// still in effect here; without a pinned/extension/override decision the
/// trust store / `defaultProjectTrust` policy decides exactly as before.
///
/// Like [`load_pi_config_with`], a malformed/unreadable/non-object settings
/// file degrades to that scope's defaults with a recorded diagnostic instead
/// of failing: the run proceeds (PIR-BIN-071).
pub fn load_pi_environment(project_dir: Option<&Path>) -> AiResult<PiEnvironment> {
    let global_dir = settings::agent_dir();
    let project_trusted = match project_dir {
        Some(cwd) => crate::extensions::project_trust::resolve_project_trust(cwd, &global_dir),
        None => false,
    };
    load_pi_environment_from(settings::load_settings(project_dir, project_trusted))
}

/// [`load_pi_environment`] against an ALREADY-loaded settings snapshot: the
/// CLI resolved both scopes (and project trust) once at startup, so the TUI
/// and RPC hosts must not re-read them here. The environment still registers
/// the full built-in catalog, unlike [`load_pi_config_with_settings`], so a
/// CLI `--model` can select a built-in provider that is not listed in
/// `models.json`.
pub fn load_pi_environment_with_settings(
    manager: &crate::config::settings::SettingsManager,
) -> AiResult<PiEnvironment> {
    load_pi_environment_from(manager.settings.clone())
}

/// Register the built-in catalog except the providers immediately composed
/// from `models.json` (or the Claude Code anthropic bootstrap) by the caller.
/// The composed provider overwrites the registration, so building the
/// intermediate one is a full model-list clone plus adapter construction that
/// is discarded at once — 480 models for an `openrouter` entry. The registry
/// is keyed by provider id (`BTreeMap`), so skipping the intermediate entry
/// cannot change lookup or enumeration order.
fn register_builtin_catalog_excluding(
    registry: &mut ProviderRegistry,
    excluded: impl Fn(&str) -> bool,
) -> AiResult<()> {
    let providers = super::catalog::load_all_shared()?;
    for entry in providers.iter() {
        if excluded(&entry.id) {
            continue;
        }
        let provider = if entry.id == "radius" {
            super::radius::RadiusProvider::from_catalog(entry)?
        } else {
            super::construction::CatalogProviderEntry::from_catalog(entry)?
        };
        registry.set_provider(provider);
    }
    Ok(())
}

fn load_pi_environment_from(mut settings_snapshot: Settings) -> AiResult<PiEnvironment> {
    // Malformed `models.json` degrades exactly like in [`load_pi_config_from`]
    // (PIR-BIN-098): empty layer + recorded warning, catalog intact.
    let (models_json, models_json_error) = match read_pi_models_json() {
        Some(contents) => {
            let (config, error) = parse_models_json_layer(&contents);
            (Some(config), error)
        }
        None => (None, None),
    };
    record_models_json_load_error(models_json_error);

    // Seed the default provider/model from models.json when settings.json does
    // not name them (upstream `applyModelsJson` + `findInitialModel`
    // precedence: settings.json, then the models.json top-level keys, then the
    // provider's `model`, then the first composed model). Without this the
    // environment-based resolution used by the TUI/RPC would ignore
    // `defaultProvider`/`defaultModel` and pick the first catalog model.
    if let Some(config) = &models_json {
        if settings_snapshot.default_provider.is_none() {
            settings_snapshot.default_provider = config
                .default_provider
                .clone()
                .or_else(|| config.providers.keys().next().cloned());
        }
        if settings_snapshot.default_model.is_none() {
            settings_snapshot.default_model = config.default_model.clone().or_else(|| {
                let provider = settings_snapshot.default_provider.as_deref()?;
                config.providers.get(provider)?.model.clone()
            });
        }
    }

    // Per-provider resolved API keys (models.json templates).
    let mut api_keys = BTreeMap::new();
    if let Some(config) = &models_json {
        for (provider_id, provider_config) in &config.providers {
            if let Some(key) = provider_config
                .api_key
                .as_deref()
                .and_then(|key| resolve_config_value(key, None))
            {
                api_keys.insert(provider_id.clone(), key);
            }
        }
    }

    // Registry: built-in catalog first, then models.json providers on top
    // (registry upsert order keeps models.json entries winning per provider).
    // Each models.json provider COMPOSES with its built-in catalog entry
    // (upstream `applyModelsJson`): catalog models seed the list, config
    // entries upsert onto it, `modelOverrides` patch the merged list. Only
    // providers without a built-in entry synthesize the localhost default.
    //
    // A provider composed below replaces its built-in registration, so skip
    // building that intermediate provider: for a configured builtin entry
    // (openrouter: 480 models) `register_builtin_catalog` would deep-clone its
    // whole model list and construct its adapters only to be overwritten.
    // `claude_env` bootstraps the anthropic provider the same way.
    let claude_env = ClaudeCodeEnvConfig::from_process_env();
    let mut registry = auth_backed_registry();
    register_builtin_catalog_excluding(&mut registry, |provider_id| {
        (claude_env.is_some() && provider_id == "anthropic")
            || models_json
                .as_ref()
                .is_some_and(|config| config.providers.contains_key(provider_id))
    })?;
    let agent_dir = settings::agent_dir();
    if let Some(config) = &models_json {
        for (provider_id, provider_config) in &config.providers {
            let builtin = super::catalog::load_provider_cached(provider_id)?;
            let (mut models, provider_headers) = compose_provider_models(
                provider_id,
                provider_config,
                builtin.as_deref(),
                &agent_dir,
            )?;
            if models.is_empty() {
                // Legacy single-model provider entry (e.g. a local Ollama
                // endpoint) with no model list and no built-in entry:
                // synthesize it.
                let base_url = provider_config
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "http://localhost:11434/v1".to_string());
                let model_id = provider_config
                    .model
                    .clone()
                    .or_else(|| provider_config.models.first().map(|m| m.id.clone()))
                    .unwrap_or_else(|| "default".to_string());
                let entry = provider_config
                    .models
                    .iter()
                    .find(|m| m.id == model_id)
                    .cloned()
                    .unwrap_or(PiModelConfig {
                        id: model_id.clone(),
                        ..PiModelConfig::default()
                    });
                let mut synthesized = pi_model_from_config(provider_id, &entry, None, None, None);
                synthesized.base_url = base_url;
                // The provider-level header layer applies to a legacy
                // single-model entry exactly like to a composed catalog model.
                if let Some(provider_headers) = provider_headers.as_ref() {
                    apply_provider_headers(&mut synthesized, provider_headers);
                }
                models = vec![synthesized];
            }
            let display_name = provider_config.name.clone().unwrap_or_else(|| {
                builtin
                    .as_deref()
                    .map(|builtin| builtin.display_name.clone())
                    .unwrap_or_else(|| provider_id.clone())
            });
            let provider =
                provider_from_owned_models(provider_id, &display_name, models, builtin.is_some())?;
            registry.set_provider(provider);
        }
    }

    // Claude Code environment compatibility: bootstrap the anthropic provider
    // from ANTHROPIC_BASE_URL / ANTHROPIC_MODEL / the alias model ids. Explicit
    // models.json values win; the environment only fills gaps.
    if let Some(env) = &claude_env {
        let empty_config = PiModelsConfig::default();
        let config = models_json.as_ref().unwrap_or(&empty_config);
        bootstrap_anthropic_env(&mut registry, config, env)?;
        if let Some((provider, model)) = claude_env_default(&settings_snapshot, config, env) {
            // The environment-selected model is the default only when neither
            // settings.json nor models.json declared one.
            settings_snapshot.default_provider = Some(provider);
            settings_snapshot.default_model = Some(model);
        }
    }

    // Available model list: everything the registry now holds (composed
    // models.json providers + untouched built-ins + env-bootstrapped anthropic).
    let models: Vec<Model> = registry.get_models();

    Ok(PiEnvironment {
        registry,
        models,
        api_keys,
        settings: settings_snapshot,
    })
}

/// Whether a provider has configured auth: a models.json API key, a provider
/// environment variable, or a stored `auth.json` credential (upstream
/// `hasConfiguredAuth`).
pub fn has_configured_auth(provider_id: &str, api_keys: &BTreeMap<String, String>) -> bool {
    if api_keys.contains_key(provider_id) {
        return true;
    }
    if crate::ai::auth::env_api_keys::all()
        .into_iter()
        .any(|(id, keys)| id == provider_id && keys.iter().any(|key| std::env::var(key).is_ok()))
    {
        return true;
    }
    crate::ai::auth::storage::FileAuthStorage::default_storage()
        .read_stored(provider_id)
        .map(|stored| stored.is_some())
        .unwrap_or(false)
}
