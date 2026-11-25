//! JSON-schema strict (constrained) sampling helpers shared by the adapters.
//!
//! Rust equivalent of upstream `packages/ai/src/api/constrained-sampling.ts`:
//! converting a tool schema to the strict subset providers expect
//! ([`make_strict_json_schema`]) and deciding whether a tool is sent in strict
//! mode ([`resolve_json_schema_strict_sampling`]).

use serde_json::{Map, Value};
use std::collections::BTreeMap;

use crate::ai::error::{AiError, AiResult};
use crate::ai::types::{ConstrainedSamplingConfig, StrictPreference, Tool};

/// Schema keys strict mode rejects outright (upstream
/// `UNSUPPORTED_STRICT_SCHEMA_KEYS`).
const UNSUPPORTED_STRICT_SCHEMA_KEYS: [&str; 16] = [
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

/// Returns true when a provider's strict mode rejects this schema keyword with
/// this value (upstream `UnsupportedStrictSchemaKeywordCheck`).
type UnsupportedStrictKeywordCheck = fn(key: &str, value: &Value) -> bool;

/// Anthropic's strict-mode keyword rejects (upstream
/// `isAnthropicStrictUnsupportedKeyword` in `anthropic-messages.ts`): numeric
/// bounds, `uniqueItems`, `min/max*`, and string `format`s outside the
/// supported RFC-3339/URI/hostname set.
pub fn anthropic_strict_unsupported_keyword(key: &str, value: &Value) -> bool {
    const UNSUPPORTED_KEYWORDS: [&str; 11] = [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "maxItems",
        "uniqueItems",
        "minContains",
        "maxContains",
        "minProperties",
        "maxProperties",
    ];
    const STRING_FORMATS: [&str; 10] = [
        "date-time",
        "time",
        "date",
        "duration",
        "email",
        "hostname",
        "uri",
        "ipv4",
        "ipv6",
        "uuid",
    ];
    if UNSUPPORTED_KEYWORDS.contains(&key) {
        return true;
    }
    if key == "minItems" {
        return !matches!(value.as_u64(), Some(0) | Some(1));
    }
    if key == "format" {
        return !value.as_str().is_some_and(|f| STRING_FORMATS.contains(&f));
    }
    false
}

fn strict_error(message: impl Into<String>) -> AiError {
    AiError::NonRetryable(format!(
        "strict JSON-schema constrained sampling: {}",
        message.into()
    ))
}

fn is_json_schema_object(value: &Value) -> bool {
    value.is_object()
}

fn schema_is_structured(schema: &Value) -> bool {
    let Some(obj) = schema.as_object() else {
        return false;
    };
    let types: Vec<&str> = match obj.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    types.contains(&"object")
        || types.contains(&"array")
        || obj.contains_key("properties")
        || obj.contains_key("items")
}

fn schema_allows_null(schema: &Value) -> bool {
    let Some(obj) = schema.as_object() else {
        return false;
    };
    match obj.get("type") {
        Some(Value::String(t)) if t == "null" => return true,
        Some(Value::Array(types)) if types.iter().any(|t| t == "null") => return true,
        _ => {}
    }
    if matches!(obj.get("const"), Some(Value::Null)) {
        return true;
    }
    if let Some(Value::Array(enum_values)) = obj.get("enum") {
        if enum_values.iter().any(Value::is_null) {
            return true;
        }
    }
    if let Some(Value::Array(variants)) = obj.get("anyOf") {
        if variants.iter().any(schema_allows_null) {
            return true;
        }
    }
    false
}

fn check_unsupported_keywords(
    schema: &Map<String, Value>,
    check: Option<UnsupportedStrictKeywordCheck>,
) -> AiResult<()> {
    let Some(check) = check else {
        return Ok(());
    };
    for (key, value) in schema {
        if check(key, value) {
            return Err(strict_error(format!(
                "{key}: {} is unsupported",
                serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
            )));
        }
    }
    Ok(())
}

/// Convert a schema to the strict subset expected by provider constrained
/// sampling (upstream `makeJsonSchemaNodeStrict`): every property becomes
/// required, `additionalProperties` is forced to `false`, and optional
/// non-nullable properties are wrapped in `anyOf` with `{"type":"null"}`.
/// Fails with a descriptive [`AiError`] when the schema cannot be expressed in
/// the strict subset.
pub fn make_strict_json_schema(
    schema: &Value,
    unsupported_keyword: Option<UnsupportedStrictKeywordCheck>,
) -> AiResult<Value> {
    let mut cloned = schema.clone();
    if !is_json_schema_object(&cloned) {
        return Err(strict_error("root schema must have type object"));
    }
    make_node_strict(&mut cloned, unsupported_keyword)?;
    if cloned.get("type").and_then(Value::as_str) != Some("object") {
        return Err(strict_error("root schema must have type object"));
    }
    Ok(cloned)
}

fn make_node_strict(
    schema: &mut Value,
    unsupported_keyword: Option<UnsupportedStrictKeywordCheck>,
) -> AiResult<()> {
    if !is_json_schema_object(schema) {
        return Err(strict_error("boolean schemas are unsupported"));
    }
    let obj = schema.as_object_mut().expect("checked above");
    for key in UNSUPPORTED_STRICT_SCHEMA_KEYS {
        if obj.contains_key(key) {
            return Err(strict_error(format!("{key} schemas are unsupported")));
        }
    }
    check_unsupported_keywords(obj, unsupported_keyword)?;

    // Upstream `makeJsonSchemaNodeStrict` recurses *in place*: the `anyOf`
    // variants and `items` schema it validates are the same values it later
    // returns. Validate owned clones here and the recursion's work (`required`
    // expansion, `additionalProperties: false` on object elements) would be
    // discarded, which is exactly the `edit` tool's `edits.items` regression.
    if let Some(any_of) = obj.get_mut("anyOf") {
        let Value::Array(variants) = any_of else {
            return Err(strict_error("anyOf must contain at least one schema"));
        };
        if variants.is_empty() {
            return Err(strict_error("anyOf must contain at least one schema"));
        }
        for variant in variants.iter_mut() {
            if schema_is_structured(variant) {
                return Err(strict_error("object and array unions are unsupported"));
            }
            make_node_strict(variant, unsupported_keyword)?;
        }
    }

    if let Some(items) = obj.get_mut("items") {
        if items.is_array() {
            return Err(strict_error("tuple schemas are unsupported"));
        }
        make_node_strict(items, unsupported_keyword)?;
    }

    let is_object_schema = obj.get("type").and_then(Value::as_str) == Some("object");
    if obj.contains_key("properties") && !is_object_schema {
        return Err(strict_error("properties require type object"));
    }
    if !is_object_schema {
        return Ok(());
    }
    match obj.get("additionalProperties") {
        Some(Value::Bool(false)) | None => {}
        Some(Value::Object(_)) | Some(Value::Bool(true)) => {
            return Err(strict_error(
                "schema-valued or true additionalProperties is unsupported",
            ));
        }
        Some(_) => {}
    }
    if let Some(properties) = obj.get("properties") {
        if !properties.is_object() {
            return Err(strict_error("object properties must be a schema map"));
        }
    }
    if let Some(required) = obj.get("required") {
        let valid = required
            .as_array()
            .is_some_and(|keys| keys.iter().all(Value::is_string));
        if !valid {
            return Err(strict_error("object required must be a string array"));
        }
    }

    let empty = Map::new();
    let properties = match obj.get_mut("properties") {
        Some(Value::Object(map)) => std::mem::take(map),
        _ => empty,
    };
    let mut properties = properties;
    let property_names: Vec<String> = properties.keys().cloned().collect();
    let required_keys: Vec<String> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|keys| {
            keys.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    for key in &required_keys {
        if !property_names.contains(key) {
            return Err(strict_error("required contains an unknown property"));
        }
    }
    let required_set: std::collections::BTreeSet<&str> =
        required_keys.iter().map(String::as_str).collect();

    for (name, property) in properties.iter_mut() {
        make_node_strict(property, unsupported_keyword)?;
        if !required_set.contains(name.as_str()) && !schema_allows_null(property) {
            *property = serde_json::json!({ "anyOf": [property.take(), { "type": "null" }] });
        }
    }
    obj.insert(
        "properties".into(),
        Value::Object(std::mem::take(&mut properties)),
    );
    obj.insert(
        "required".into(),
        Value::Array(property_names.into_iter().map(Value::String).collect()),
    );
    obj.insert("additionalProperties".into(), Value::Bool(false));
    Ok(())
}

/// Tool parameters after the strict transform (upstream
/// `getJsonSchemaToolParameters`): strict tools get the transformed schema,
/// everything else is passed through verbatim.
pub fn get_json_schema_tool_parameters(tool: &Tool, strict: bool) -> AiResult<Value> {
    if strict {
        make_strict_json_schema(&tool.parameters, None)
    } else {
        Ok(tool.parameters.clone())
    }
}

/// Decide whether a JSON-schema tool is sent in strict mode (upstream
/// `resolveJsonSchemaStrictSampling`). Returns `Some(true)` when the strict
/// transform succeeds, `None` when strict mode is not applicable (tool has no
/// JSON-schema config, strict is unsupported, or a "prefer" tool fails the
/// transform), and an error when a "require" tool cannot be made strict.
pub fn resolve_json_schema_strict_sampling(
    tool: &Tool,
    supports_strict_mode: bool,
    unsupported_keyword: Option<UnsupportedStrictKeywordCheck>,
) -> AiResult<Option<bool>> {
    let Some(ConstrainedSamplingConfig::JsonSchema { strict }) = &tool.constrained_sampling else {
        return Ok(None);
    };
    if supports_strict_mode {
        match make_strict_json_schema(&tool.parameters, unsupported_keyword) {
            Ok(_) => return Ok(Some(true)),
            Err(error) => {
                if matches!(strict, StrictPreference::Require) {
                    return Err(AiError::NonRetryable(format!(
                        "Tool \"{}\" requires JSON-schema constrained sampling, but {}.",
                        tool.name,
                        error_message(&error)
                    )));
                }
                return Ok(None);
            }
        }
    }
    if matches!(strict, StrictPreference::Require) {
        return Err(AiError::NonRetryable(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        )));
    }
    Ok(None)
}

fn error_message(error: &AiError) -> String {
    match error {
        AiError::NonRetryable(message) => message.clone(),
        _ => error.to_string(),
    }
}

/// A resolved OpenAI grammar-constrained tool (upstream
/// `GrammarConstrainedSampling`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarConstrainedSampling {
    /// Grammar syntax: `"lark"` or `"regex"`.
    pub format: &'static str,
    /// The grammar definition sent in the custom-tool `format.grammar`.
    pub definition: String,
    /// The single required string property carrying the tool input.
    pub input_property: String,
}

/// The single required string property of a grammar tool (upstream
/// `inferGrammarInputProperty`).
fn infer_grammar_input_property(tool: &Tool) -> AiResult<String> {
    let schema = &tool.parameters;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(AiError::NonRetryable(
            "grammar constrained sampling requires an object parameter schema".to_string(),
        ));
    }
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AiError::NonRetryable(
                "grammar constrained sampling requires exactly one required string property"
                    .to_string(),
            )
        })?;
    if required.len() != 1 {
        return Err(AiError::NonRetryable(
            "grammar constrained sampling requires exactly one required string property"
                .to_string(),
        ));
    }
    let Some(input_property) = required.first().and_then(Value::as_str) else {
        return Err(AiError::NonRetryable(
            "grammar constrained sampling requires exactly one required string property"
                .to_string(),
        ));
    };
    let property = schema
        .get("properties")
        .and_then(|properties| properties.get(input_property))
        .ok_or_else(|| {
            AiError::NonRetryable(format!(
                "grammar constrained sampling requires a properties entry for {input_property}"
            ))
        })?;
    if property.get("type").and_then(Value::as_str) != Some("string") {
        return Err(AiError::NonRetryable(format!(
            "grammar constrained sampling property {input_property} must have type string"
        )));
    }
    Ok(input_property.to_string())
}

/// Resolve a tool to OpenAI custom-tool grammar constrained sampling (upstream
/// `resolveGrammarConstrainedSampling`): grammar-configured tools convert only
/// when the provider supports the format and an `openai_lark`/`openai_regex`
/// variant is provided.
pub fn resolve_grammar_constrained_sampling(
    tool: &Tool,
    supports_openai_grammar_tools: bool,
) -> AiResult<Option<GrammarConstrainedSampling>> {
    let Some(ConstrainedSamplingConfig::Grammar { variants }) = &tool.constrained_sampling else {
        return Ok(None);
    };
    if !supports_openai_grammar_tools {
        return Ok(None);
    }
    let lark = variants
        .get("openai_lark")
        .map(|definition| definition.trim())
        .filter(|definition| !definition.is_empty());
    let regex = variants
        .get("openai_regex")
        .map(|definition| definition.trim())
        .filter(|definition| !definition.is_empty());
    let (format, definition) = match (lark, regex) {
        (Some(definition), _) => ("lark", definition),
        (None, Some(definition)) => ("regex", definition),
        (None, None) => {
            return Err(AiError::NonRetryable(format!(
                "Tool \"{}\" cannot use grammar constrained sampling: no supported grammar \
                 variant was provided.",
                tool.name
            )));
        }
    };
    let input_property = infer_grammar_input_property(tool).map_err(|error| {
        AiError::NonRetryable(format!(
            "Tool \"{}\" cannot use grammar constrained sampling: {}.",
            tool.name,
            error_message(&error)
        ))
    })?;
    Ok(Some(GrammarConstrainedSampling {
        format,
        definition: definition.to_string(),
        input_property,
    }))
}

/// Map tool name -> grammar input property for the declared tools (upstream
/// `createGrammarToolInputProperties`).
pub fn create_grammar_tool_input_properties(
    tools: &[Tool],
    supports_openai_grammar_tools: bool,
) -> AiResult<BTreeMap<String, String>> {
    let mut properties = BTreeMap::new();
    for tool in tools {
        if let Some(grammar) =
            resolve_grammar_constrained_sampling(tool, supports_openai_grammar_tools)?
        {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    Ok(properties)
}
