//! Generated configuration JSON Schemas (upstream
//! `scripts/generate-schemas.ts` + `packages/coding-agent/schemas/*`).
//!
//! The committed artifacts under `schemas/` are produced by the functions in
//! this module; `tests/config_schemas.rs` asserts the committed files match
//! the generated output (upstream `config-schemas.test.ts` freshness gate).
//! Draft 2020-12 with stable `$id`s.
//!
//! The published `$id`/`$schema` reference URL is `https://pi.dev/schemas/*`
//! (upstream `#10751`: the generator keeps the URL and the repository output
//! path separate, so the committed artifacts still live under `schemas/`).

use serde_json::{json, Value};

/// Base of the PUBLISHED schema URLs (upstream `generate-schemas.ts`
/// `schemaBaseUrl`), distinct from the repository path the artifacts are
/// committed under ([`render_config_schemas`]).
const SCHEMA_BASE_URL: &str = "https://pi.dev/schemas";

/// The `$schema` reference URL for settings.json.
pub const SCHEMA_BASE_URL_SETTINGS: &str = "https://pi.dev/schemas/settings.schema.json";
/// The `$schema` reference URL for models.json.
pub const SCHEMA_BASE_URL_MODELS: &str = "https://pi.dev/schemas/models.schema.json";
/// The `$schema` reference URL for keybindings.json.
pub const SCHEMA_BASE_URL_KEYBINDINGS: &str = "https://pi.dev/schemas/keybindings.schema.json";
/// The `$schema` reference URL for theme files.
pub const SCHEMA_BASE_URL_THEMES: &str = "https://pi.dev/schemas/theme.schema.json";

fn schema_id(name: &str) -> String {
    format!("{SCHEMA_BASE_URL}/{name}.schema.json")
}

fn string_prop(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn string_min(description: &str, min: usize) -> Value {
    json!({ "type": "string", "minLength": min, "description": description })
}

fn bool_prop(description: &str) -> Value {
    json!({ "type": "boolean", "description": description })
}

/// A boolean property carrying the runtime default, so the published schema
/// and the settings accessors cannot drift.
fn bool_prop_with_default(description: &str, default: bool) -> Value {
    json!({ "type": "boolean", "description": description, "default": default })
}

fn non_negative_safe_integer(description: Option<&str>, default: Option<u64>) -> Value {
    let mut schema = json!({
        "type": "integer",
        "minimum": 0,
        "maximum": 9_007_199_254_740_991_u64,
    });
    if let Some(description) = description {
        schema["description"] = json!(description);
    }
    if let Some(default) = default {
        schema["default"] = json!(default);
    }
    schema
}

fn string_array(description: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": description })
}

fn enum_prop(values: &[&str], description: Option<&str>, default: Option<&str>) -> Value {
    let mut schema = json!({ "enum": values });
    if let Some(description) = description {
        schema["description"] = json!(description);
    }
    if let Some(default) = default {
        schema["default"] = json!(default);
    }
    schema
}

/// `number | "disabled"` timeout setting.
fn timeout_setting(description: &str) -> Value {
    json!({
        "anyOf": [
            { "type": "number", "minimum": 0 },
            { "const": "disabled" },
        ],
        "description": description,
    })
}

/// The header map (upstream `patternProperties: { "^.*$": { "type": "string" } }`;
/// pir's loader also accepts JSON `null` as a suppression marker).
fn header_record(description: &str) -> Value {
    json!({
        "type": "object",
        "patternProperties": {
            "^.*$": { "type": "string" },
        },
        "description": description,
    })
}

fn compat_schema() -> Value {
    json!({
        "$ref": "#/$defs/ProviderCompat",
        "description": "Provider and model compatibility overrides.",
    })
}

/// The `thinkingLevelMap` description, published at each `$ref` site
/// (upstream keeps the seven levels in `$defs.ThinkingLevelMap`).
const THINKING_LEVEL_MAP_DESCRIPTION: &str = "Provider value sent for each thinking level. null marks a level unsupported. Levels the model does not map use the provider's default value through high; xhigh and max are unsupported.";

/// Prompt-cache description, published at each `$ref` site.
const PROMPT_CACHE_DESCRIPTION: &str = "Best-effort prompt cache lifetime in seconds for the short and long retention tiers. Pi warms only tiers with a known lifetime.";

fn thinking_level_map_ref() -> Value {
    json!({
        "$ref": "#/$defs/ThinkingLevelMap",
        "description": THINKING_LEVEL_MAP_DESCRIPTION,
    })
}

fn thinking_level_map_def() -> Value {
    let levels = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
    let properties: serde_json::Map<String, Value> = levels
        .iter()
        .map(|level| {
            (
                level.to_string(),
                json!({ "anyOf": [{ "type": "string" }, { "type": "null" }] }),
            )
        })
        .collect();
    json!({
        "type": "object",
        "properties": Value::Object(properties),
    })
}

fn sampling_params() -> Value {
    json!({
        "type": "object",
        "patternProperties": {
            "^.*$": {},
        },
        "description": "Fields merged into every request body. Applies only to OpenAI-compatible APIs.",
    })
}

fn sampling_params_by_thinking_level() -> Value {
    let levels = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
    let properties: serde_json::Map<String, Value> = levels
        .iter()
        .map(|level| {
            (
                level.to_string(),
                json!({
                    "type": "object",
                    "patternProperties": {
                        "^.*$": {},
                    },
                }),
            )
        })
        .collect();
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "description": "samplingParams per thinking level, merged over samplingParams.",
    })
}

fn prompt_cache_ref() -> Value {
    json!({
        "$ref": "#/$defs/ModelPromptCache",
        "description": PROMPT_CACHE_DESCRIPTION,
    })
}

fn prompt_cache_def() -> Value {
    json!({
        "type": "object",
        "properties": {
            "short": { "type": "number", "exclusiveMinimum": 0 },
            "long": { "type": "number", "exclusiveMinimum": 0 },
        },
    })
}

fn model_input_limits() -> Value {
    json!({
        "type": "object",
        "properties": {
            "maxRequestBytes": {
                "type": "integer",
                "minimum": 1,
                "description": "Maximum serialized provider request size in bytes.",
            },
            "images": {
                "type": "object",
                "properties": {
                    "resize": {
                        "type": "object",
                        "properties": {
                            "maxWidth": { "type": "integer", "minimum": 1 },
                            "maxHeight": { "type": "integer", "minimum": 1 },
                            "maxBytes": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "Maximum base64-encoded payload size in bytes.",
                            },
                            "jpegQuality": { "type": "integer", "minimum": 1, "maximum": 100 },
                        },
                        "description": "Cache-safe resize profile applied before a new image enters conversation history.",
                    },
                    "maxPerMessage": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum images accepted in one provider message.",
                    },
                    "maxPerRequest": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum images accepted across one provider request.",
                    },
                },
            },
        },
    })
}

/// How one `ProviderCompat` property is rendered from the data table.
enum CompatField {
    /// `boolean` with an optional schema `default`.
    Bool(Option<bool>),
    /// `anyOf` of `{ "type": "string", "const": <value> }` members.
    Enum(&'static [&'static str]),
    /// A single `const` string.
    Const(&'static str),
    /// A `number` with no bounds.
    Number,
    /// A nested schema built by [`compat_nested_schema`].
    Nested(CompatNested),
}

/// The nested `ProviderCompat` schemas (hand-shaped, mirroring upstream
/// `packages/ai/src/providers/compat-schema.ts`).
#[derive(Clone, Copy)]
enum CompatNested {
    ChatTemplateKwargs,
    ChatTemplateArgs,
    OpenRouterRouting,
    VercelGatewayRouting,
    AllowedFallbackModels,
    SamplingParamsByThinkingLevel,
    CustomHeaders,
    InputLimits,
    PromptCache,
}

/// `(name, description, field)` for every upstream `ProviderCompat` knob,
/// in upstream declaration order (45 entries). Deliberately unknown keys are
/// preserved by `additionalProperties: true`.
const COMPAT_FIELDS: &[(&str, Option<&str>, CompatField)] = &[
    ("supportsStore", Some("Whether the provider supports the store field. Default: auto-detected from URL."), CompatField::Bool(None)),
    ("supportsDeveloperRole", Some("Whether the provider supports the developer role instead of system. Defaults are API-specific."), CompatField::Bool(None)),
    ("supportsReasoningEffort", Some("Whether the provider supports reasoning_effort. Default: auto-detected from URL."), CompatField::Bool(None)),
    ("supportsUsageInStreaming", Some("Whether the provider supports stream_options.include_usage for token usage in streaming responses."), CompatField::Bool(Some(true))),
    ("supportsFinishReason", Some("Whether streamed responses include finish_reason. When false, pi infers stop or toolUse when the stream ends."), CompatField::Bool(Some(true))),
    ("maxTokensField", Some("Which field to use for max tokens. Default: auto-detected from URL."), CompatField::Enum(&["max_completion_tokens", "max_tokens"])),
    ("requiresToolResultName", Some("Whether tool results require the name field. Default: auto-detected from URL."), CompatField::Bool(None)),
    ("requiresAssistantAfterToolResult", Some("Whether a user message after tool results requires an assistant message in between. Default: auto-detected from URL."), CompatField::Bool(None)),
    ("requiresThinkingAsText", Some("Whether thinking blocks must be converted to text blocks with <thinking> delimiters. Default: auto-detected from URL."), CompatField::Bool(None)),
    ("requiresReasoningContentOnAssistantMessages", Some("Whether all replayed assistant messages must include an empty reasoning_content field when reasoning is enabled. Default: auto-detected from URL."), CompatField::Bool(None)),
    ("thinkingFormat", Some("Format for reasoning or thinking parameters. When omitted, Pi auto-detects the format from the provider URL. \"openai\" uses reasoning_effort, \"openrouter\" uses reasoning.effort, \"deepseek\" uses thinking.type plus reasoning_effort when supported, \"together\" uses reasoning.enabled plus reasoning_effort when supported, \"baseten\" uses configurable chat_template_args plus reasoning_effort when supported, \"zai\" uses thinking.type, \"qwen\" uses top-level enable_thinking, \"qwen-chat-template\" uses chat_template_kwargs.enable_thinking and preserve_thinking, \"chat-template\" uses configurable chat_template_kwargs, \"string-thinking\" uses top-level thinking, and \"ant-ling\" uses reasoning.effort only when the mapped effort is non-null."), CompatField::Enum(&["openai", "openrouter", "deepseek", "together", "baseten", "zai", "qwen", "chat-template", "qwen-chat-template", "string-thinking", "ant-ling"])),
    ("chatTemplateKwargs", Some("Kwargs sent as chat_template_kwargs when thinkingFormat is \"chat-template\". Use $var with \"thinking.enabled\", \"thinking.effort\", or \"thinking.budget\" for pi-controlled values."), CompatField::Nested(CompatNested::ChatTemplateKwargs)),
    ("chatTemplateArgs", Some("Arguments sent as chat_template_args when thinkingFormat is \"baseten\". Use $var with \"thinking.enabled\", \"thinking.effort\", or \"thinking.budget\" for pi-controlled values."), CompatField::Nested(CompatNested::ChatTemplateArgs)),
    ("openRouterRouting", Some("OpenRouter provider routing preferences. Controls which upstream providers OpenRouter routes requests to. Sent as the provider field in the OpenRouter API request body. See https://openrouter.ai/docs/guides/routing/provider-selection."), CompatField::Nested(CompatNested::OpenRouterRouting)),
    ("vercelGatewayRouting", Some("Vercel AI Gateway routing preferences. Controls which upstream providers the gateway routes requests to. See https://vercel.com/docs/ai-gateway/models-and-providers/provider-options."), CompatField::Nested(CompatNested::VercelGatewayRouting)),
    ("zaiToolStream", Some("Whether z.ai supports top-level tool_stream for streaming tool call deltas."), CompatField::Bool(Some(false))),
    ("thinkingTokenBudgetField", Some("Top-level request field used to cap reasoning tokens from thinkingBudgets. Reasoning and the answer share max_tokens on these endpoints. \"thinking_token_budget\" is vLLM, \"thinking_budget\" is Qwen, DashScope, or SGLang, and \"thinking_budget_tokens\" is llama.cpp. Off by default and not set on the generated catalog."), CompatField::Enum(&["thinking_token_budget", "thinking_budget", "thinking_budget_tokens"])),
    ("supportsThinkingTokenBudget", Some("Alias for thinkingTokenBudgetField: \"thinking_token_budget\" (vLLM). Prefer thinkingTokenBudgetField."), CompatField::Bool(Some(false))),
    ("supportsOpenAIGrammarTools", Some("Whether the provider supports OpenAI custom tools with Lark or regex grammar formats. When false, grammar-constrained tools fall back to normal function tools."), CompatField::Bool(Some(false))),
    ("supportsMidConvoSystemMessages", Some("Whether the exact model accepts system or developer messages after the conversation has started. When false, later system messages are folded into the leading system message."), CompatField::Bool(Some(false))),
    ("supportsMidConvoToolAdditions", Some("Whether system messages can introduce additional tools mid-conversation. Requires supportsMidConvoSystemMessages. The generated catalog enables this for capable models."), CompatField::Bool(Some(false))),
    ("supportsStrictMode", Some("Whether the provider supports strict tool schemas. Defaults are API-specific."), CompatField::Bool(None)),
    ("cacheControlFormat", Some("Cache control convention for prompt caching. Anthropic applies cache_control markers to the system prompt, last tool definition, and last user, assistant, or tool-result text content."), CompatField::Const("anthropic")),
    ("sendSessionAffinityHeaders", Some("Whether to send session-affinity data from options.sessionId. Defaults are API-specific."), CompatField::Bool(None)),
    ("sessionAffinityFormat", Some("Session-affinity header format. Defaults are API-specific or auto-detected."), CompatField::Enum(&["openai", "openai-nosession", "openrouter"])),
    ("supportsLongCacheRetention", Some("Whether the provider supports long prompt cache retention. Defaults are API-specific or auto-detected."), CompatField::Bool(None)),
    ("vllmPriority", Some("vLLM scheduler priority sent as the top-level priority request field. Lower values are handled earlier and the server default is 0. Only meaningful with --scheduling-policy priority. Off by default and not set on the generated catalog."), CompatField::Number),
    ("supportsAdditionalTools", Some("Whether the model supports message-anchored additional_tools input items."), CompatField::Bool(Some(false))),
    ("supportsToolSearch", Some("Whether the model supports client-executed tool search for transcript-anchored additions."), CompatField::Bool(Some(false))),
    ("supportsExplicitPromptCacheMode", Some("Whether the model accepts prompt_cache_options. Older OpenAI models reject the parameter."), CompatField::Bool(Some(false))),
    ("supportsMidConvoEffort", Some("Whether thinking level changes are sent inside the conversation instead of changing the request's effort, so a change keeps the prompt cache. Used by the anthropic-messages and openai-responses APIs."), CompatField::Bool(Some(false))),
    ("supportsMaxOutputTokens", Some("Whether the provider accepts max_output_tokens. Some Codex-protocol gateways reject it."), CompatField::Bool(Some(true))),
    ("supportsEagerToolInputStreaming", Some("Whether the provider accepts per-tool eager_input_streaming. When false, the Anthropic provider omits tools[].eager_input_streaming and sends the legacy fine-grained-tool-streaming-2025-05-14 beta header for tool-enabled requests."), CompatField::Bool(Some(true))),
    ("supportsCacheControlOnTools", Some("Whether the provider supports Anthropic-style cache_control markers on tool definitions. When false, cache_control is omitted from tool parameters. Some Anthropic-compatible providers, such as Fireworks, do not support this field on tools and may reject or ignore it."), CompatField::Bool(Some(true))),
    ("supportsTemperature", Some("Whether the model accepts the Anthropic temperature request field. Claude Opus 4.7+ rejects non-default values."), CompatField::Bool(Some(true))),
    ("forceAdaptiveThinking", Some("Whether to force adaptive thinking (thinking.type: adaptive plus output_config.effort) regardless of model ID. Built-in models that require adaptive thinking set this in generated metadata. Custom Anthropic-compatible providers can set this to true for any model whose upstream requires the adaptive format. Set false to opt out on overridden built-in models."), CompatField::Bool(Some(false))),
    ("allowEmptySignature", Some("Whether to replay empty thinking signatures instead of converting thinking to text."), CompatField::Bool(Some(false))),
    ("supportsStrictTools", Some("Whether the provider supports Anthropic strict tool schemas. Generated Anthropic models enable it explicitly."), CompatField::Bool(Some(false))),
    ("supportsMidConvoToolChanges", Some("Whether the exact model accepts mid-conversation tool_addition and tool_removal blocks. Requires supportsMidConvoSystemMessages."), CompatField::Bool(Some(false))),
    ("allowedFallbackModels", Some("Models Anthropic accepts for server-side refusal fallback, with local pricing metadata for returned fallback responses. When absent or empty, callers must omit fallbacks; Anthropic rejects the field for models with no permitted fallback targets."), CompatField::Nested(CompatNested::AllowedFallbackModels)),
    ("samplingParamsByThinkingLevel", Some("samplingParams per thinking level, merged over samplingParams."), CompatField::Nested(CompatNested::SamplingParamsByThinkingLevel)),
    ("customHeaders", Some("Extra static headers appended to provider requests."), CompatField::Nested(CompatNested::CustomHeaders)),
    ("inputLimits", None, CompatField::Nested(CompatNested::InputLimits)),
    ("promptCache", Some("Best-effort prompt cache lifetime in seconds for the short and long retention tiers. Pi warms only tiers with a known lifetime."), CompatField::Nested(CompatNested::PromptCache)),
    ("authHeader", Some("The api key is sent as Authorization: Bearer <apiKey> instead of the provider's default credential header."), CompatField::Bool(None)),
];

/// `anyOf` members for a provider-compat enum knob (upstream renders each
/// member as `{ "type": "string", "const": <value> }`).
fn const_union(values: &[&'static str]) -> Vec<Value> {
    values
        .iter()
        .map(|value| json!({ "type": "string", "const": value }))
        .collect()
}

/// `{ "type": <kind>, "description": <description> }` node.
fn described(kind: &str, description: &str) -> Value {
    json!({ "type": kind, "description": description })
}

/// `{ "type": "string" }` node.
fn string_schema() -> Value {
    json!({ "type": "string" })
}

/// `{ "type": "number" }` node.
fn number_schema() -> Value {
    json!({ "type": "number" })
}

/// `number | string` price field (OpenRouter routing `max_price`).
fn price_field(description: &str) -> Value {
    json!({
        "anyOf": [number_schema(), string_schema()],
        "description": description,
    })
}

/// `number | { p50, p75, p90, p99 }`, where `prefix` completes each
/// percentile description (OpenRouter routing throughput/latency knobs).
fn percentile_field(description: &str, prefix: &str) -> Value {
    json!({
        "anyOf": [
            number_schema(),
            {
                "type": "object",
                "properties": {
                    "p50": described("number", &format!("{prefix} at the 50th percentile.")),
                    "p75": described("number", &format!("{prefix} at the 75th percentile.")),
                    "p90": described("number", &format!("{prefix} at the 90th percentile.")),
                    "p99": described("number", &format!("{prefix} at the 99th percentile.")),
                },
            },
        ],
        "description": description,
    })
}

/// `array of string` node with a description.
fn described_string_array(description: &str) -> Value {
    json!({
        "type": "array",
        "items": string_schema(),
        "description": description,
    })
}

/// `{ "type": "string", "const": <value> }` node.
fn const_string(value: &str) -> Value {
    json!({ "type": "string", "const": value })
}

/// The `chat_template_kwargs` / `chat_template_args` map: a value is a JSON
/// scalar or a `$var` control object.
fn chat_template_map() -> Value {
    json!({
        "type": "object",
        "patternProperties": {
            "^.*$": {
                "anyOf": [
                    string_schema(),
                    number_schema(),
                    { "type": "boolean" },
                    { "type": "null" },
                    {
                        "type": "object",
                        "required": ["$var"],
                        "properties": {
                            "$var": {
                                "anyOf": [
                                    const_string("thinking.enabled"),
                                    const_string("thinking.effort"),
                                    const_string("thinking.budget"),
                                ],
                            },
                            "omitWhenOff": { "type": "boolean" },
                        },
                    },
                ],
            },
        },
    })
}

fn compat_nested_schema(nested: CompatNested) -> Value {
    match nested {
        // Both knobs share one shape; only the table description differs.
        CompatNested::ChatTemplateKwargs | CompatNested::ChatTemplateArgs => chat_template_map(),
        CompatNested::OpenRouterRouting => json!({
            "type": "object",
            "properties": {
                "allow_fallbacks": {
                    "type": "boolean",
                    "description": "Whether to allow backup providers to serve requests.",
                    "default": true,
                },
                "require_parameters": {
                    "type": "boolean",
                    "description": "Whether to filter providers to only those that support all parameters in the request.",
                    "default": false,
                },
                "data_collection": {
                    "anyOf": [const_string("deny"), const_string("allow")],
                    "description": "Data collection setting. \"allow\": allow providers that may store or train on data. \"deny\": only use providers that do not collect user data.",
                    "default": "allow",
                },
                "zdr": {
                    "type": "boolean",
                    "description": "Whether to restrict routing to only ZDR (Zero Data Retention) endpoints.",
                },
                "enforce_distillable_text": {
                    "type": "boolean",
                    "description": "Whether to restrict routing to only models that allow text distillation.",
                },
                "order": described_string_array(
                    "An ordered list of provider names or slugs to try in sequence, falling back to the next if unavailable.",
                ),
                "only": described_string_array(
                    "List of provider names or slugs to exclusively allow for this request.",
                ),
                "ignore": described_string_array(
                    "List of provider names or slugs to skip for this request.",
                ),
                "quantizations": described_string_array(
                    "A list of quantization levels to filter providers by, for example [\"fp16\", \"bf16\", \"fp8\", \"fp6\", \"int8\", \"int4\", \"fp4\", \"fp32\"].",
                ),
                "sort": {
                    "anyOf": [
                        string_schema(),
                        {
                            "type": "object",
                            "properties": {
                                "by": described(
                                    "string",
                                    "The sorting metric, such as \"price\", \"throughput\", or \"latency\".",
                                ),
                                "partition": {
                                    "anyOf": [string_schema(), { "type": "null" }],
                                    "description": "Partitioning strategy: \"model\" or \"none\".",
                                    "default": "model",
                                },
                            },
                        },
                    ],
                    "description": "Sorting strategy. Can be a string such as \"price\", \"throughput\", or \"latency\", or an object.",
                },
                "max_price": {
                    "type": "object",
                    "properties": {
                        "prompt": price_field("Price per million prompt tokens."),
                        "completion": price_field("Price per million completion tokens."),
                        "image": price_field("Price per image."),
                        "audio": price_field("Price per audio unit."),
                        "request": price_field("Price per request."),
                    },
                    "description": "Maximum price per million tokens in USD.",
                },
                "preferred_min_throughput": percentile_field(
                    "Preferred minimum throughput in tokens per second. A number applies to p50.",
                    "Minimum tokens per second",
                ),
                "preferred_max_latency": percentile_field(
                    "Preferred maximum latency in seconds. A number applies to p50.",
                    "Maximum latency in seconds",
                ),
            },
            "description": "OpenRouter provider routing preferences. Controls which upstream providers OpenRouter routes requests to. Sent as the provider field in the OpenRouter API request body. See https://openrouter.ai/docs/guides/routing/provider-selection.",
        }),
        CompatNested::VercelGatewayRouting => json!({
            "type": "object",
            "properties": {
                "only": described_string_array(
                    "List of provider slugs to exclusively use for this request, for example [\"bedrock\", \"anthropic\"].",
                ),
                "order": described_string_array(
                    "List of provider slugs to try in order, for example [\"anthropic\", \"openai\"].",
                ),
            },
            "description": "Vercel AI Gateway routing preferences. Controls which upstream providers the gateway routes requests to. See https://vercel.com/docs/ai-gateway/models-and-providers/provider-options.",
        }),
        CompatNested::AllowedFallbackModels => json!({
            "type": "array",
            "items": {
                "type": "object",
                "required": ["provider", "model", "cost"],
                "properties": {
                    "provider": { "type": "string", "minLength": 1 },
                    "model": { "type": "string", "minLength": 1 },
                    "cost": { "$ref": "#/$defs/ModelCost" },
                },
                "description": "An Anthropic server-side refusal fallback model with local pricing metadata.",
            },
            "maxItems": 3,
            "description": "Models Anthropic accepts for server-side refusal fallback, with local pricing metadata for returned fallback responses. When absent or empty, callers must omit fallbacks; Anthropic rejects the field for models with no permitted fallback targets.",
        }),
        CompatNested::SamplingParamsByThinkingLevel => sampling_params_by_thinking_level(),
        CompatNested::CustomHeaders => {
            header_record("Extra static headers appended to provider requests.")
        }
        CompatNested::InputLimits => json!({ "$ref": "#/$defs/ModelInputLimits" }),
        CompatNested::PromptCache => json!({ "$ref": "#/$defs/ModelPromptCache" }),
    }
}

fn provider_compat_schema() -> Value {
    let mut properties = serde_json::Map::new();
    for (name, description, field) in COMPAT_FIELDS {
        let mut schema = match field {
            CompatField::Bool(default) => {
                let mut schema = json!({ "type": "boolean" });
                if let Some(default) = default {
                    schema["default"] = json!(default);
                }
                schema
            }
            CompatField::Enum(values) => json!({ "anyOf": const_union(values) }),
            CompatField::Const(value) => json!({ "type": "string", "const": value }),
            CompatField::Number => json!({ "type": "number" }),
            CompatField::Nested(nested) => compat_nested_schema(*nested),
        };
        if let Some(field_description) = description {
            schema["description"] = json!(field_description);
        }
        properties.insert((*name).to_string(), schema);
    }
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "additionalProperties": true,
    })
}

// ---------------------------------------------------------------------------
// settings.schema.json
// ---------------------------------------------------------------------------

/// Build the settings.json JSON Schema (upstream
/// `core/settings-schema.ts`).
pub fn settings_schema() -> Value {
    let compaction_model_override = json!({
        "type": "object",
        "properties": {
            "reserveTokens": non_negative_safe_integer(None, None),
            "keepRecentTokens": non_negative_safe_integer(None, None),
        },
        "additionalProperties": false,
    });
    let mut properties = serde_json::Map::new();
    properties.insert("$schema".into(), string_prop("JSON Schema reference."));
    properties.insert(
        "lastChangelogVersion".into(),
        string_prop("The changelog version last shown."),
    );
    properties.insert(
        "defaultProvider".into(),
        string_prop("Default provider id."),
    );
    properties.insert(
        "defaultDisabledPackagesEnabled".into(),
        bool_prop(
            "Opt in to the packages pir keeps disabled by default even when installed and \
             listed in \"packages\" (currently @malinamnam/pi-phone: it starts a local web \
             server and uses about 14 MB of QuickJS heap). Global settings scope only; an \
             explicit -e/--extension path naming the package also enables it.",
        ),
    );
    properties.insert("defaultModel".into(), string_prop("Default model id."));
    properties.insert(
        "defaultThinkingLevel".into(),
        enum_prop(
            &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
            Some("Default thinking level for new sessions."),
            None,
        ),
    );
    properties.insert(
        "modelThinkingLevels".into(),
        json!({
            "type": "object",
            "additionalProperties": enum_prop(
                &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
                None,
                None,
            ),
            "description": "Per-model default thinking level overrides keyed by \"provider/modelId\".",
        }),
    );
    properties.insert(
        "transport".into(),
        enum_prop(
            &["auto", "sse", "websocket", "websocket-cached"],
            Some("Provider transport protocol."),
            Some("auto"),
        ),
    );
    properties.insert(
        "sessionShareDestination".into(),
        enum_prop(
            &["auto", "gist", "radius"],
            Some(
                "Destination used by /share. \"auto\" uses Radius when authenticated and otherwise creates a GitHub gist.",
            ),
            Some(crate::config::defaults::settings_defaults::SESSION_SHARE_DESTINATION),
        ),
    );
    properties.insert(
        "steeringMode".into(),
        enum_prop(
            &["all", "one-at-a-time"],
            Some("How steering messages queue."),
            Some("one-at-a-time"),
        ),
    );
    properties.insert(
        "followUpMode".into(),
        enum_prop(
            &["all", "one-at-a-time"],
            Some("How follow-up messages queue."),
            Some("one-at-a-time"),
        ),
    );
    properties.insert("theme".into(), string_prop("Theme name or path."));
    properties.insert(
        "compaction".into(),
        json!({
            "type": "object",
            "properties": {
                "enabled": bool_prop_with_default(
                    "Enable automatic compaction.",
                    crate::config::defaults::settings_defaults::COMPACTION_ENABLED,
                ),
                "reserveTokens": non_negative_safe_integer(None, Some(16_384)),
                "keepRecentTokens": non_negative_safe_integer(None, Some(20_000)),
                "modelOverrides": {
                    "type": "object",
                    "additionalProperties": compaction_model_override,
                    "description": "Per-model overrides keyed by exact \"provider/modelId\" strings.",
                },
            },
            "additionalProperties": false,
        }),
    );
    properties.insert(
        "branchSummary".into(),
        json!({
            "type": "object",
            "properties": {
                "reserveTokens": {
                    "type": "number",
                    "description": "Tokens reserved for the prompt and LLM response.",
                    "default": 16_384,
                },
                "skipPrompt": {
                    "type": "boolean",
                    "description": "When true, skips the \"Summarize branch?\" prompt and defaults to no summary.",
                    "default": false,
                },
            },
            "additionalProperties": false,
        }),
    );
    properties.insert(
        "retry".into(),
        json!({
            "type": "object",
            "properties": {
                "enabled": { "type": "boolean", "default": true },
                "maxRetries": { "type": "number", "default": 3 },
                "baseDelayMs": {
                    "type": "number",
                    "description": "Exponential backoff base delay in milliseconds: 2s, 4s, 8s.",
                    "default": 2000,
                },
                "maxAgentDelayMs": { "type": "number", "default": 60_000 },
                "provider": {
                    "type": "object",
                    "properties": {
                        "timeoutMs": {
                            "type": "number",
                            "description": "SDK or provider request timeout in milliseconds.",
                        },
                        "maxRetries": {
                            "type": "number",
                            "description": "SDK or provider retry attempts.",
                        },
                        "maxRetryDelayMs": {
                            "type": "number",
                            "description": "Maximum server-requested delay before failing.",
                            "default": 60_000,
                        },
                    },
                    "additionalProperties": false,
                },
                "maxDelayMs": {
                    "type": "number",
                    "description": "Legacy retry delay setting. Use provider.maxRetryDelayMs instead.",
                    "deprecated": true,
                },
            },
            "additionalProperties": false,
        }),
    );
    properties.insert(
        "hideThinkingBlock".into(),
        bool_prop_with_default(
            "Hide collapsed thinking blocks in the transcript.",
            crate::config::defaults::settings_defaults::HIDE_THINKING_BLOCK,
        ),
    );
    properties.insert(
        "showCacheMissNotices".into(),
        bool_prop_with_default(
            "Show cache cost and provider recovery notices.",
            crate::config::defaults::settings_defaults::SHOW_CACHE_MISS_NOTICES,
        ),
    );
    properties.insert(
        "externalEditor".into(),
        string_prop("Command for Ctrl+G external editor; takes precedence over VISUAL and EDITOR."),
    );
    properties.insert(
        "shellPath".into(),
        string_prop("Custom shell path, for example for Cygwin on Windows, with support for leading ~ expansion."),
    );
    properties.insert(
        "quietStartup".into(),
        json!({
            "anyOf": [
                { "type": "boolean" },
                { "const": "header" },
            ],
            "description": "When true, hide all startup output. When \"header\", keep only the startup header.",
            "default": false,
        }),
    );
    properties.insert(
        "defaultProjectTrust".into(),
        json!({
            "enum": ["ask", "always", "never"],
            "description": "Global setting only.",
            "default": "ask",
        }),
    );
    properties.insert(
        "shellCommandPrefix".into(),
        string_prop("Prefix prepended to every bash command, for example to enable shell aliases."),
    );
    properties.insert(
        "shellPool".into(),
        bool_prop(
            "Run bash and powershell commands through a persistent pooled shell process \
             (one long-lived shell reused across calls, commands isolated in subshells). \
             Disable to spawn a fresh shell process per command.",
        ),
    );
    properties.insert(
        "tools".into(),
        json!({
            "type": "object",
            "description": "Per-tool execution settings (pir extension; upstream ignores this object).",
            "properties": {
                "commandTimeoutMs": non_negative_safe_integer(
                    Some(
                        "Default timeout in milliseconds for a bash/powershell command that \
                         does not pass its own timeout. Unset applies 120000 (120 seconds), a \
                         deliberate deviation from upstream's no-default-timeout so one hung \
                         command cannot consume an entire run; set 0 to restore upstream \
                         semantics. An explicit per-call timeout always wins. When exceeded, \
                         the command's process tree is killed and the tool returns an error \
                         naming this setting.",
                    ),
                    Some(crate::config::settings::DEFAULT_COMMAND_TIMEOUT_MS),
                ),
            },
            "additionalProperties": false,
        }),
    );
    properties.insert(
        "npmCommand".into(),
        string_array("Command used for npm package lookup and installation, in argv form such as [\"mise\", \"exec\", \"node@20\", \"--\", \"npm\"]."),
    );
    properties.insert(
        "collapseChangelog".into(),
        bool_prop_with_default(
            "Show the condensed changelog after update; use /changelog for the full changelog.",
            crate::config::defaults::settings_defaults::COLLAPSE_CHANGELOG,
        ),
    );
    properties.insert(
        "enableInstallTelemetry".into(),
        bool_prop_with_default(
            "Send an anonymous version and update ping after changelog-detected updates.",
            crate::config::defaults::settings_defaults::ENABLE_INSTALL_TELEMETRY,
        ),
    );
    properties.insert(
        "enableAnalytics".into(),
        bool_prop_with_default(
            "Opt in to analytics data sharing.",
            crate::config::defaults::settings_defaults::ENABLE_ANALYTICS,
        ),
    );
    properties.insert(
        "trackingId".into(),
        string_prop("Analytics tracking identifier, generated when analytics is enabled."),
    );
    properties.insert(
        "packages".into(),
        json!({
            "type": "array",
            "items": {
                "anyOf": [
                    string_prop("Load all resources from the package."),
                    {
                        "type": "object",
                        "properties": {
                            "source": { "type": "string" },
                            "autoload": bool_prop("When false, start empty and only apply explicit resource patterns."),
                            "extensions": string_array("Extension paths or package globs."),
                            "skills": string_array("Skill paths or package globs."),
                            "prompts": string_array("Prompt template paths or package globs."),
                            "themes": string_array("Theme paths or package globs."),
                        },
                        "required": ["source"],
                        "additionalProperties": false,
                    },
                ],
                "description": "Package source for npm or git packages. Use the string form to load all resources or the object form to filter resources.",
            },
            "description": "npm or git package sources, as strings or objects with resource filtering.",
        }),
    );
    properties.insert(
        "extensions".into(),
        string_array("Local extension file paths or directories."),
    );
    properties.insert(
        "disabledBuiltinExtensions".into(),
        string_array(
            "Built-in extensions not to load, by name (such as \"mcp\"), even when settings or -e enable them; the settings spelling of --no-mcp. Names from the global and project scopes are unioned: a project can add a restriction but never re-enable a builtin disabled globally.",
        ),
    );
    properties.insert(
        "deviceId".into(),
        string_prop(
            "Stable installation UUID, generated when authentication first needs it. Global only.",
        ),
    );
    properties.insert(
        "skills".into(),
        json!({
            "anyOf": [
                string_array("Local skill file paths or directories."),
                {
                    "type": "object",
                    "properties": {
                        "enableSkillCommands": { "type": "boolean" },
                        "customDirectories": string_array("Legacy skills directories."),
                    },
                    "additionalProperties": false,
                    "deprecated": true,
                },
            ],
            "description": "Local skill file paths or directories.",
        }),
    );
    properties.insert(
        "prompts".into(),
        string_array("Local prompt template file paths or directories."),
    );
    properties.insert(
        "themes".into(),
        string_array("Local theme file paths or directories."),
    );
    properties.insert(
        "enableSkillCommands".into(),
        bool_prop_with_default(
            "Register skills as /skill:name commands.",
            crate::config::defaults::settings_defaults::ENABLE_SKILL_COMMANDS,
        ),
    );
    properties.insert(
        "terminal".into(),
        json!({
            "type": "object",
            "properties": {
                "showImages": {
                    "type": "boolean",
                    "description": "Show images when the terminal supports them.",
                    "default": true,
                },
                "imageWidthCells": {
                    "type": "number",
                    "description": "Preferred inline image width in terminal cells.",
                    "default": 60,
                },
                "clearOnShrink": {
                    "type": "boolean",
                    "description": "Clear empty rows when content shrinks.",
                    "default": false,
                },
                "showTerminalProgress": {
                    "type": "boolean",
                    "description": "Show OSC 9;4 terminal progress indicators.",
                    "default": false,
                },
                "hyperlinks": {
                    "anyOf": [{ "type": "boolean" }, { "const": "auto" }],
                },
                "images": {
                    "enum": ["kitty", "iterm2", "auto", false],
                },
                "trueColor": {
                    "anyOf": [{ "type": "boolean" }, { "const": "auto" }],
                },
            },
            "additionalProperties": false,
        }),
    );
    properties.insert(
        "images".into(),
        json!({
            "type": "object",
            "properties": {
                "autoResize": {
                    "type": "boolean",
                    "description": "Resize images to 2000x2000 maximum for better model compatibility.",
                    "default": true,
                },
                "blockImages": {
                    "type": "boolean",
                    "description": "When true, prevents all images from being sent to LLM providers.",
                    "default": false,
                },
            },
            "additionalProperties": false,
        }),
    );
    properties.insert(
        "enabledModels".into(),
        string_array("Model patterns for cycling, in the same format as the --models CLI flag."),
    );
    properties.insert(
        "defaultTools".into(),
        string_array("Initial tool selection. Plain names replace the inherited selection; +name and -name entries add or remove tools."),
    );
    properties.insert(
        "doubleEscapeAction".into(),
        json!({
            "enum": ["fork", "tree", "none"],
            "description": "Action for double-escape with an empty editor.",
            "default": "tree",
        }),
    );
    properties.insert(
        "treeFilterMode".into(),
        json!({
            "enum": ["default", "no-tools", "user-only", "labeled-only", "all"],
            "description": "Default filter when opening /tree.",
            "default": "default",
        }),
    );
    properties.insert(
        "thinkingBudgets".into(),
        json!({
            "type": "object",
            "properties": {
                "minimal": { "type": "number" },
                "low": { "type": "number" },
                "medium": { "type": "number" },
                "high": { "type": "number" },
            },
            "additionalProperties": false,
            "description": "Custom token budgets for thinking levels.",
        }),
    );
    properties.insert(
        "editorPaddingX".into(),
        json!({
            "type": "number",
            "description": "Horizontal padding for the input editor.",
            "default": 0,
        }),
    );
    properties.insert(
        "outputPad".into(),
        json!({
            "enum": [0, 1],
            "description": "Horizontal padding for transcript content.",
            "default": 1,
        }),
    );
    properties.insert(
        "autocompleteMaxVisible".into(),
        json!({
            "type": "number",
            "description": "Maximum visible items in the autocomplete dropdown.",
            "default": 5,
        }),
    );
    properties.insert(
        "showHardwareCursor".into(),
        bool_prop("Show the terminal cursor while still positioning it for IME."),
    );
    properties.insert(
        "markdown".into(),
        json!({
            "type": "object",
            "properties": {
                "codeBlockIndent": {
                    "type": "string",
                    "default": "  ",
                },
                "mermaid": {
                    "enum": ["off", "final", "streaming"],
                    "default": "streaming",
                },
            },
            "additionalProperties": false,
        }),
    );
    properties.insert(
        "warnings".into(),
        json!({
            "type": "object",
            "properties": {
                "anthropicExtraUsage": {
                    "type": "boolean",
                    "default": true,
                },
            },
            "additionalProperties": false,
        }),
    );
    properties.insert(
        "codemode".into(),
        json!({
            "type": "object",
            "properties": {
                "mode": {
                    "enum": ["on", "only"],
                    "description": "How codemode presents tools. \"on\" keeps direct tools declared and annotates tools callable from scripts; codemode lists only tools without direct exposure. \"only\" lists every script-callable tool in codemode and does not declare active direct tools.",
                    "default": "on",
                },
                "inlineBudget": {
                    "type": "number",
                    "minimum": 0,
                    "description": "Estimated tokens available for inline codemode tool declarations.",
                    "default": 3000,
                },
                "sandboxMemoryMb": {
                    "type": "number",
                    "minimum": 0,
                    "description": "QuickJS heap cap for one codemode script execution, in MiB (0 = no cap). Applied to both the in-process and worker-process sandboxes.",
                    "default": 128,
                },
            },
            "additionalProperties": false,
        }),
    );
    properties.insert(
        "extensionLimits".into(),
        json!({
            "type": "object",
            "properties": {
                "memoryMb": {
                    "type": "number",
                    "minimum": 0,
                    "description": "QuickJS heap cap for the shared extension runtime, in MiB (0 = no cap). An over-limit allocation fails that dispatch/tool call with a clear error; the host survives.",
                },
                "stackMb": {
                    "type": "number",
                    "minimum": 0,
                    "description": "JS stack size in MiB. 0 keeps the 16 MiB default; values above 16 are clamped to 16 (a larger value would disable QuickJS's stack check).",
                },
                "gcThresholdMb": {
                    "type": "number",
                    "minimum": 0,
                    "description": "QuickJS GC threshold in MiB (0 = QuickJS default pacing).",
                },
                "callTimeoutMs": {
                    "type": "number",
                    "minimum": 0,
                    "description": "Per-call timeout in milliseconds for extension dispatch and tool execution (default 30000; 0 keeps the default).",
                },
                "hookTimeoutMs": {
                    "type": "number",
                    "minimum": 0,
                    "description": "Timeout in milliseconds for reserved-prefix hook calls (__pir_stream: / __pir_route:; default 1800000; 0 keeps the default).",
                },
                "beforeAgentStartTimeoutMs": {
                    "type": "number",
                    "minimum": 0,
                    "description": "Budget for the before_agent_start lifecycle dispatch in milliseconds; 0 keeps the default (10 s). When it elapses the host stops waiting, the run proceeds, and the late handler's completion is discarded.",
                },
            },
            "additionalProperties": false,
            "description": "QuickJS resource limits of the extension runtime. Global settings scope only: project .pi/settings.json cannot loosen these. PI_EXT_* environment variables override them.",
        }),
    );
    properties.insert(
        "pythonTool".into(),
        json!({
            "type": "boolean",
            "description": "Register the embedded RustPython 3 `python` tool (pir-only). Set false to skip it — for example when a custom `python` extension or the native-python path is preferred. `disabledBuiltinExtensions: [\"python\"]` removes the builtin at the loader layer regardless.",
            "default": true,
        }),
    );
    properties.insert(
        "sessionDir".into(),
        string_prop(
            "Custom session storage directory, in the same format as the --session-dir CLI flag.",
        ),
    );
    properties.insert(
        "httpProxy".into(),
        string_prop("Proxy URL applied as HTTP_PROXY and HTTPS_PROXY for Pi-managed HTTP clients."),
    );
    properties.insert(
        "httpIdleTimeoutMs".into(),
        timeout_setting(
            "HTTP header or body idle timeout in milliseconds; 0 or \"disabled\" disables it.",
        ),
    );
    properties.insert(
        "cacheWarming".into(),
        json!({
            "enum": ["off", "streaming", "idle"],
            "description": "Cache-warming profile. \"idle\" also warms between agent runs. Global only because each refresh costs money.",
            "default": "streaming",
        }),
    );
    properties.insert(
        "websocketConnectTimeoutMs".into(),
        timeout_setting("WebSocket connect or open handshake timeout in milliseconds; 0 or \"disabled\" disables it."),
    );
    properties.insert(
        "tuiMode".into(),
        json!({
            "enum": ["regular", "fullscreen"],
            "default": "fullscreen",
        }),
    );
    properties.insert(
        "fullscreenExitOutput".into(),
        json!({
            "enum": ["transcript", "resume-hint"],
            "description": "No effect in regular TUI mode.",
            "default": "transcript",
        }),
    );
    properties.insert(
        "fullscreenScrollbar".into(),
        json!({
            "enum": ["auto", "always", "hidden"],
            "description": "No effect in regular TUI mode.",
            "default": "auto",
        }),
    );
    properties.insert(
        "fullscreenCopyOnSelect".into(),
        json!({
            "type": "boolean",
            "description": "No effect in regular TUI mode.",
            "default": true,
        }),
    );
    properties.insert(
        "fullscreenWheelScrollLines".into(),
        json!({
            "anyOf": [{ "type": "number" }, { "const": "auto" }],
            "description": "Lines scrolled per wheel event in fullscreen mode; numeric values are clamped from 1 to 100.",
            "default": "auto",
        }),
    );
    properties.insert(
        "keybindings".into(),
        json!({
            "type": "object",
            "additionalProperties": {
                "anyOf": [
                    { "type": "string" },
                    { "type": "array", "items": { "type": "string" } },
                ],
            },
            "description": "TUI keybinding overrides; pir also reads keybindings.json.",
        }),
    );
    // Deprecated legacy fields (valid input, migrated on load).
    properties.insert(
        "queueMode".into(),
        json!({
            "enum": ["all", "one-at-a-time"],
            "description": "Legacy setting migrated to steeringMode.",
            "deprecated": true,
        }),
    );
    properties.insert(
        "websockets".into(),
        json!({
            "type": "boolean",
            "description": "Legacy setting migrated to transport.",
            "deprecated": true,
        }),
    );

    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": schema_id("settings"),
        "title": "Pi Coding Agent Settings",
        "description": "Settings for Pi coding agent",
        "type": "object",
        "properties": Value::Object(properties),
        "additionalProperties": true,
    })
}

// ---------------------------------------------------------------------------
// models.schema.json
// ---------------------------------------------------------------------------

/// The four flat rate fields shared by `$defs.ModelCost` and the inline
/// `modelOverrides[].cost` shape (upstream keeps both `type: number` with no
/// lower bound).
fn cost_rate_properties() -> Value {
    json!({
        "input": { "type": "number", "description": "Input cost in USD per million tokens." },
        "output": { "type": "number", "description": "Output cost in USD per million tokens." },
        "cacheRead": { "type": "number", "description": "Cache-read cost in USD per million tokens." },
        "cacheWrite": { "type": "number", "description": "Cache-write cost in USD per million tokens." },
    })
}

/// Request-wide pricing tiers (upstream requires every rate on a tier; the
/// highest matching threshold applies to the full request).
fn cost_tiers_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "required": ["inputTokensAbove", "input", "output", "cacheRead", "cacheWrite"],
            "properties": {
                "inputTokensAbove": {
                    "type": "number",
                    "description": "Use this tier when total request input exceeds this token count.",
                },
                "input": { "type": "number", "description": "Input cost in USD per million tokens." },
                "output": { "type": "number", "description": "Output cost in USD per million tokens." },
                "cacheRead": { "type": "number", "description": "Cache-read cost in USD per million tokens." },
                "cacheWrite": { "type": "number", "description": "Cache-write cost in USD per million tokens." },
            },
        },
        "description": "Request-wide pricing tiers. The highest matching input threshold applies to the full request.",
    })
}

/// `$defs.ModelCost`: the complete rate table a `models[]` entry publishes
/// (upstream `ModelCostSchema`, referenced by `models[].cost` and
/// `compat.allowedFallbackModels[].cost`).
fn model_cost_def() -> Value {
    let mut properties = cost_rate_properties();
    properties["tiers"] = cost_tiers_schema();
    json!({
        "type": "object",
        "required": ["input", "output", "cacheRead", "cacheWrite"],
        "properties": properties,
    })
}

/// A `modelOverrides[].cost` patch (upstream keeps this inline and requires no
/// rate, because an override merges per field).
fn model_override_cost() -> Value {
    let mut properties = cost_rate_properties();
    properties["tiers"] = cost_tiers_schema();
    json!({
        "type": "object",
        "properties": properties,
    })
}

/// A `models[]` entry (upstream `ModelDefinitionSchema`), with pir's model
/// fields (all deserializable into `PiModelConfig`).
fn model_entry_schema() -> Value {
    json!({
        "type": "object",
        "required": ["id"],
        "properties": {
            "id": {
                "type": "string",
                "minLength": 1,
                "description": "Model ID sent to the API.",
            },
            "name": string_min("Display name, also matched by --model patterns.", 1),
            "api": string_min("Overrides the provider api.", 1),
            "baseUrl": string_min("Overrides the provider baseUrl.", 1),
            "reasoning": {
                "type": "boolean",
                "default": false,
                "description": "Whether the model supports thinking.",
            },
            "thinkingLevelMap": thinking_level_map_ref(),
            "input": {
                "type": "array",
                "items": { "enum": ["text", "image"] },
                "default": ["text"],
            },
            "inputLimits": { "$ref": "#/$defs/ModelInputLimits" },
            "cost": { "$ref": "#/$defs/ModelCost" },
            "promptCache": prompt_cache_ref(),
            "contextWindow": {
                "type": "number",
                "exclusiveMinimum": 0,
                "default": 128_000,
                "description": "Context window in tokens.",
            },
            "maxTokens": {
                "type": "number",
                "exclusiveMinimum": 0,
                "default": 16_384,
                "description": "Maximum output tokens.",
            },
            "samplingParams": sampling_params(),
            "samplingParamsByThinkingLevel": sampling_params_by_thinking_level(),
            "headers": header_record("Request headers. Values resolve like apiKey."),
            "compat": compat_schema(),
        },
    })
}

/// A `modelOverrides` entry: upstream's partial model shape (no `required`),
/// plus the `id` / `baseUrl` / `api` keys pir's `apply_pi_model_override`
/// honors.
fn model_override_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": string_min("Display name, also matched by --model patterns.", 1),
            "reasoning": {
                "type": "boolean",
                "description": "Whether the model supports thinking.",
            },
            "thinkingLevelMap": thinking_level_map_ref(),
            "input": {
                "type": "array",
                "items": { "enum": ["text", "image"] },
            },
            "inputLimits": { "$ref": "#/$defs/ModelInputLimits" },
            "cost": model_override_cost(),
            "promptCache": prompt_cache_ref(),
            "contextWindow": {
                "type": "number",
                "exclusiveMinimum": 0,
                "description": "Context window in tokens.",
            },
            "maxTokens": {
                "type": "number",
                "exclusiveMinimum": 0,
                "description": "Maximum output tokens.",
            },
            "samplingParams": sampling_params(),
            "samplingParamsByThinkingLevel": sampling_params_by_thinking_level(),
            "headers": header_record("Request headers. Values resolve like apiKey."),
            "compat": compat_schema(),
            // pir-only override keys (upstream's override shape omits them;
            // `additionalProperties` stays open on both sides).
            "id": { "type": "string" },
            "baseUrl": string_min("Overrides the provider baseUrl.", 1),
            "api": string_min("Overrides the provider api.", 1),
        },
    })
}

/// Build the models.json JSON Schema (upstream `core/model-config.ts`).
///
/// Shape follows upstream `packages/coding-agent/schemas/models.schema.json`:
/// the provider map and its nested maps use `patternProperties`, the shared
/// shapes live in `$defs` (`ModelCost`, `ModelInputLimits`, `ModelPromptCache`,
/// `ProviderCompat`, `ThinkingLevelMap`), and the root carries the generated
/// file `$comment` instead of a title/description with
/// `additionalProperties: false`. Residual differences (pir-only compat fields,
/// override keys) are recorded in `schemas/models.schema.json.NOTES.md`.
pub fn models_schema() -> Value {
    let provider = json!({
        "type": "object",
        "properties": {
            "name": string_min("Display name.", 1),
            "baseUrl": string_min(
                "API base URL. Also applies to the provider's built-in models.",
                1,
            ),
            "apiKey": string_min(
                "Literal key, \"$NAME\" environment interpolation, or \"!command\" output.",
                1,
            ),
            "api": string_min(
                "API for the provider's models, such as \"openai-completions\", \"openai-responses\", \"anthropic-messages\", or \"google-generative-ai\".",
                1,
            ),
            "oauth": {
                "type": "string",
                "const": "radius",
                "description": "OAuth login flow. Requires baseUrl.",
            },
            "headers": header_record("Request headers. Values resolve like apiKey."),
            "compat": compat_schema(),
            "authHeader": {
                "type": "boolean",
                "default": false,
                "description": "Whether to send the resolved API key as Authorization: Bearer.",
            },
            "models": {
                "type": "array",
                "items": model_entry_schema(),
            },
            "modelOverrides": {
                "type": "object",
                "patternProperties": {
                    "^.*$": model_override_schema(),
                },
                "description": "Per-model overrides applied last, to chat models only.",
            },
        },
    });
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": schema_id("models"),
        "$comment": "This file is generated from TypeScript source. Do not edit it manually; run npm run generate:schemas.",
        "type": "object",
        "required": ["providers"],
        "properties": {
            "$schema": { "type": "string" },
            "providers": {
                "type": "object",
                "patternProperties": {
                    "^.*$": provider,
                },
            },
        },
        "$defs": {
            "ModelCost": model_cost_def(),
            "ModelInputLimits": model_input_limits(),
            "ModelPromptCache": prompt_cache_def(),
            "ProviderCompat": provider_compat_schema(),
            "ThinkingLevelMap": thinking_level_map_def(),
        },
    })
}

// ---------------------------------------------------------------------------
// keybindings.schema.json
// ---------------------------------------------------------------------------

/// Build the keybindings.json JSON Schema (upstream
/// `core/keybindings-schema.ts`).
/// The upstream keybinding action ids and their user-facing descriptions,
/// in upstream declaration order (upstream `core/keybindings.ts`
/// `APP_KEYBINDINGS` + `packages/tui/src/keybindings.ts` `TUI_KEYBINDINGS`).
///
/// The TUI registry (`crate::tui::keybindings::TUI_KEYBINDING_DEFINITIONS`)
/// and the app-layer registry (`crate::config::keybindings::
/// default_keybinding_definitions_for`) cover subsets; the published schema
/// must list every bindable id, so the generator owns the union here.
const KEYBINDING_ACTIONS: &[(&str, &str)] = &[
    ("tui.editor.cursorUp", "Move cursor up"),
    ("tui.editor.cursorDown", "Move cursor down"),
    (
        "tui.editor.historyPrevious",
        "Select previous prompt history entry",
    ),
    ("tui.editor.historyNext", "Select next prompt history entry"),
    ("tui.editor.cursorLeft", "Move cursor left"),
    ("tui.editor.cursorRight", "Move cursor right"),
    ("tui.editor.cursorWordLeft", "Move cursor word left"),
    ("tui.editor.cursorWordRight", "Move cursor word right"),
    ("tui.editor.cursorLineStart", "Move to line start"),
    ("tui.editor.cursorLineEnd", "Move to line end"),
    ("tui.editor.jumpForward", "Jump forward to character"),
    ("tui.editor.jumpBackward", "Jump backward to character"),
    ("tui.editor.pageUp", "Page up"),
    ("tui.editor.pageDown", "Page down"),
    ("tui.editor.deleteCharBackward", "Delete character backward"),
    ("tui.editor.deleteCharForward", "Delete character forward"),
    ("tui.editor.deleteWordBackward", "Delete word backward"),
    ("tui.editor.deleteWordForward", "Delete word forward"),
    ("tui.editor.deleteToLineStart", "Delete to line start"),
    ("tui.editor.deleteToLineEnd", "Delete to line end"),
    ("tui.editor.yank", "Yank"),
    ("tui.editor.yankPop", "Yank pop"),
    ("tui.editor.undo", "Undo"),
    ("tui.input.newLine", "Insert newline"),
    ("tui.input.submit", "Submit input"),
    ("tui.input.tab", "Tab / autocomplete"),
    ("tui.input.copy", "Copy selection"),
    ("tui.select.up", "Move selection up"),
    ("tui.select.down", "Move selection down"),
    ("tui.select.pageUp", "Selection page up"),
    ("tui.select.pageDown", "Selection page down"),
    ("tui.select.confirm", "Confirm selection"),
    ("tui.select.cancel", "Cancel selection"),
    ("tui.altScreen.pageUp", "Scroll viewport up one page"),
    ("tui.altScreen.pageDown", "Scroll viewport down one page"),
    ("tui.altScreen.halfPageUp", "Scroll viewport up half a page"),
    (
        "tui.altScreen.halfPageDown",
        "Scroll viewport down half a page",
    ),
    ("tui.altScreen.lineUp", "Scroll viewport up one line"),
    ("tui.altScreen.lineDown", "Scroll viewport down one line"),
    (
        "tui.altScreen.previousPrompt",
        "Jump to previous semantic prompt",
    ),
    ("tui.altScreen.nextPrompt", "Jump to next semantic prompt"),
    ("tui.altScreen.search", "Search the primary scroll view"),
    ("tui.altScreen.searchNext", "Select the next search match"),
    (
        "tui.altScreen.searchPrevious",
        "Select the previous search match",
    ),
    ("tui.altScreen.searchClose", "Close transcript search"),
    ("tui.altScreen.top", "Scroll viewport to top"),
    ("tui.altScreen.bottom", "Scroll viewport to bottom"),
    ("app.interrupt", "Cancel or abort"),
    ("app.clear", "Clear editor"),
    ("app.exit", "Exit when editor is empty"),
    ("app.suspend", "Suspend to background"),
    ("app.thinking.cycle", "Cycle thinking level"),
    ("app.thinking.save", "Save thinking level"),
    ("app.model.cycleForward", "Cycle to next model"),
    ("app.model.cycleBackward", "Cycle to previous model"),
    ("app.model.select", "Open model selector"),
    ("app.tools.expand", "Toggle tool output"),
    ("app.thinking.toggle", "Toggle thinking blocks"),
    (
        "app.session.toggleNamedFilter",
        "Toggle named session filter",
    ),
    ("app.editor.external", "Open external editor"),
    (
        "app.message.copy",
        "Copy selection or last assistant message",
    ),
    ("app.message.followUp", "Queue follow-up message"),
    ("app.message.dequeue", "Restore queued messages"),
    (
        "app.clipboard.pasteImage",
        "Paste files on macOS, images, or text from clipboard",
    ),
    ("app.session.new", "Start a new session"),
    ("app.session.tree", "Open session tree"),
    ("app.session.fork", "Fork current session"),
    ("app.session.resume", "Resume a session"),
    ("app.tree.foldOrUp", "Fold tree branch or move up"),
    ("app.tree.unfoldOrDown", "Unfold tree branch or move down"),
    ("app.tree.editLabel", "Edit tree label"),
    (
        "app.tree.toggleLabelTimestamp",
        "Toggle tree label timestamps",
    ),
    ("app.session.togglePath", "Toggle session path display"),
    ("app.session.toggleSort", "Toggle session sort mode"),
    ("app.session.rename", "Rename session"),
    ("app.session.delete", "Delete session"),
    (
        "app.session.deleteNoninvasive",
        "Delete session when query is empty",
    ),
    ("app.models.save", "Save model selection"),
    ("app.models.enableAll", "Enable all models"),
    ("app.models.clearAll", "Clear all models"),
    (
        "app.models.toggleProvider",
        "Toggle all models for provider",
    ),
    ("app.models.reorderUp", "Move model up in order"),
    ("app.models.reorderDown", "Move model down in order"),
    ("app.tree.filter.default", "Tree filter: default view"),
    ("app.tree.filter.noTools", "Tree filter: hide tool results"),
    (
        "app.tree.filter.userOnly",
        "Tree filter: user messages only",
    ),
    (
        "app.tree.filter.labeledOnly",
        "Tree filter: labeled entries only",
    ),
    ("app.tree.filter.all", "Tree filter: show all entries"),
    ("app.tree.filter.cycleForward", "Tree filter: cycle forward"),
    (
        "app.tree.filter.cycleBackward",
        "Tree filter: cycle backward",
    ),
];

pub fn keybindings_schema() -> Value {
    let named_keys = [
        "escape",
        "esc",
        "enter",
        "return",
        "tab",
        "space",
        "backspace",
        "delete",
        "insert",
        "clear",
        "home",
        "end",
        "pageUp",
        "pageDown",
        "up",
        "down",
        "left",
        "right",
        "f1",
        "f2",
        "f3",
        "f4",
        "f5",
        "f6",
        "f7",
        "f8",
        "f9",
        "f10",
        "f11",
        "f12",
        "`",
        "-",
        "=",
        "[",
        "]",
        "\\",
        ";",
        "'",
        ",",
        ".",
        "/",
        "!",
        "@",
        "#",
        "$",
        "%",
        "^",
        "&",
        "*",
        "(",
        ")",
        "_",
        "+",
        "|",
        "~",
        "{",
        "}",
        ":",
        "<",
        ">",
        "?",
    ];
    // Regex-escape every named key (upstream `escapeRegex`): metacharacters
    // such as `[`, `.`, `$`, `(` must stay literal inside the alternation.
    let escaped: Vec<String> = named_keys
        .iter()
        .map(|key| {
            let mut escaped = String::with_capacity(key.len());
            for character in key.chars() {
                if "\\^$.|?*+()[]{}".contains(character) {
                    escaped.push('\\');
                }
                escaped.push(character);
            }
            escaped
        })
        .collect();
    let base_key_pattern = format!(r#"(?:[a-z0-9]|{})"#, escaped.join("|"));
    let modifiers = ["ctrl", "shift", "alt", "super"];
    let duplicate_modifier_pattern: Vec<String> = modifiers
        .iter()
        .map(|modifier| format!(r"{modifier}\+.*{modifier}\+"))
        .collect();
    let key_id_pattern = format!(
        r#"^(?!.*(?:{}))(?:(?:{})\+){{0,4}}{}$"#,
        duplicate_modifier_pattern.join("|"),
        modifiers.join("|"),
        base_key_pattern
    );
    let key_id = json!({
        "type": "string",
        "pattern": key_id_pattern,
        "description": "Key identifier with optional ctrl, shift, alt, or super modifiers.",
    });
    let mut properties = serde_json::Map::new();
    properties.insert("$schema".into(), json!({ "type": "string" }));
    for (id, description) in KEYBINDING_ACTIONS {
        properties.insert(
            (*id).to_string(),
            json!({
                "$ref": "#/$defs/KeybindingValue",
                "description": description,
            }),
        );
    }
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": schema_id("keybindings"),
        "title": "Pi Coding Agent Keybindings",
        "description": "Custom keybindings for Pi coding agent",
        "type": "object",
        "properties": Value::Object(properties),
        "additionalProperties": { "$ref": "#/$defs/KeybindingValue" },
        "$defs": {
            "KeybindingValue": {
                "anyOf": [
                    key_id.clone(),
                    { "type": "array", "items": key_id },
                ],
            },
        },
    })
}

// ---------------------------------------------------------------------------
// theme.schema.json
// ---------------------------------------------------------------------------

/// `(name, description)` pairs for the theme `colors` object, in the upstream
/// declaration order. Entries marked `false` are optional
/// (compatible fallbacks apply).
const THEME_COLORS: &[(&str, &str, bool)] = &[
    ("accent", "Primary accent color (logo, selected items, cursor)", true),
    ("border", "Normal borders", true),
    ("borderAccent", "Highlighted borders", true),
    ("borderMuted", "Subtle borders", true),
    ("success", "Success states", true),
    ("error", "Error states", true),
    ("warning", "Warning states", true),
    ("muted", "Secondary/dimmed text", true),
    ("dim", "Very dimmed text (more subtle than muted)", true),
    ("text", "Default text color (usually empty string)", true),
    ("thinkingText", "Thinking block text color", true),
    ("scrollbarTrack", "Fullscreen scrollbar track foreground (falls back to muted when omitted)", false),
    ("scrollbarThumb", "Fullscreen scrollbar thumb foreground (falls back to text when omitted)", false),
    ("selectedBg", "Selected item background", true),
    ("searchMatchBg", "Transcript search match background and current-match text (falls back to selectedBg when omitted)", false),
    ("searchMatchText", "Transcript search match text and current-match background (falls back to text when omitted)", false),
    ("userMessageBg", "User message background", true),
    ("userMessageText", "User message text", true),
    ("customMessageBg", "Custom message background (hook-injected messages)", true),
    ("customMessageText", "Custom message text", true),
    ("customMessageLabel", "Custom message type label color", true),
    ("toolPendingBg", "Tool execution box (pending state)", true),
    ("toolSuccessBg", "Tool execution box (success state)", true),
    ("toolErrorBg", "Tool execution box (error state)", true),
    ("toolTitle", "Tool execution box title color", true),
    ("toolOutput", "Tool execution box output text color", true),
    ("mdHeading", "Markdown heading text", true),
    ("mdLink", "Markdown link text", true),
    ("mdLinkUrl", "Markdown link URL", true),
    ("mdCode", "Markdown inline code", true),
    ("mdCodeBlock", "Markdown code block content", true),
    ("mdCodeBlockBorder", "Markdown code block fences", true),
    ("mdQuote", "Markdown blockquote text", true),
    ("mdQuoteBorder", "Markdown blockquote border", true),
    ("mdHr", "Markdown horizontal rule", true),
    ("mdListBullet", "Markdown list bullets/numbers", true),
    ("toolDiffAdded", "Added lines in tool diffs", true),
    ("toolDiffRemoved", "Removed lines in tool diffs", true),
    ("toolDiffContext", "Context lines in tool diffs", true),
    ("syntaxComment", "Syntax highlighting: comments", true),
    ("syntaxKeyword", "Syntax highlighting: keywords", true),
    ("syntaxFunction", "Syntax highlighting: function names", true),
    ("syntaxVariable", "Syntax highlighting: variable names", true),
    ("syntaxString", "Syntax highlighting: string literals", true),
    ("syntaxNumber", "Syntax highlighting: number literals", true),
    ("syntaxType", "Syntax highlighting: type names", true),
    ("syntaxOperator", "Syntax highlighting: operators", true),
    ("syntaxPunctuation", "Syntax highlighting: punctuation", true),
    ("thinkingOff", "Thinking level border: off", true),
    ("thinkingMinimal", "Thinking level border: minimal", true),
    ("thinkingLow", "Thinking level border: low", true),
    ("thinkingMedium", "Thinking level border: medium", true),
    ("thinkingHigh", "Thinking level border: high", true),
    ("thinkingXhigh", "Thinking level border: xhigh", true),
    ("thinkingMax", "Thinking level border: max (falls back to thinkingXhigh when omitted)", false),
    ("bashMode", "Editor border color in bash mode", true),
];

fn color_value_ref() -> Value {
    json!({ "$ref": "#/$defs/ColorValue" })
}

/// Build the theme.json JSON Schema (upstream
/// `packages/coding-agent/schemas/theme.schema.json`).
pub fn theme_schema() -> Value {
    let mut color_properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, description, required_field) in THEME_COLORS {
        let mut property = color_value_ref();
        property["description"] = json!(description);
        color_properties.insert((*name).to_string(), property);
        if *required_field {
            required.push(json!(name));
        }
    }
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": schema_id("theme"),
        "$comment": "This file is generated from TypeScript source. Do not edit it manually; run npm run generate:schemas.",
        "type": "object",
        "required": ["name", "colors"],
        "properties": {
            "$schema": {
                "type": "string",
                "description": "JSON schema reference",
            },
            "name": {
                "type": "string",
                "pattern": "^[^/]+$",
                "description": "Theme name. Must not contain '/' because it is reserved for automatic light/dark theme settings.",
            },
            "appearance": {
                "anyOf": [
                    { "type": "string", "const": "dark" },
                    { "type": "string", "const": "light" },
                ],
                "description": "Background the theme is designed for. Detected from the theme colors when omitted.",
            },
            "vars": {
                "type": "object",
                "patternProperties": {
                    "^.*$": { "$ref": "#/$defs/ColorValue" },
                },
                "description": "Reusable color variables",
            },
            "colors": {
                "type": "object",
                "required": required,
                "properties": Value::Object(color_properties),
                "description": "Theme color definitions (scrollbar, thinkingMax, and search highlight colors are optional and use compatible fallbacks)",
                "additionalProperties": false,
            },
            "export": {
                "type": "object",
                "properties": {
                    "pageBg": {
                        "$ref": "#/$defs/ColorValue",
                        "description": "Page background color",
                    },
                    "cardBg": {
                        "$ref": "#/$defs/ColorValue",
                        "description": "Card/container background color",
                    },
                    "infoBg": {
                        "$ref": "#/$defs/ColorValue",
                        "description": "Info sections background (system prompt, notices)",
                    },
                },
                "description": "Optional colors for HTML export (defaults derived from userMessageBg if not specified)",
                "additionalProperties": false,
            },
        },
        "title": "Pi Coding Agent Theme",
        "description": "Theme schema for Pi coding agent",
        "additionalProperties": false,
        "$defs": {
            "ColorValue": {
                "anyOf": [
                    {
                        "type": "string",
                        "description": "Hex color (#RGB or #RRGGBB), OKLCH or OKHSL color, variable reference, or empty string for terminal default",
                    },
                    {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 255,
                        "description": "256-color palette index (0-255)",
                    },
                ],
            },
        },
    })
}

/// The committed artifacts, keyed by repository-relative path (upstream
/// `renderConfigSchemas`).
pub fn render_config_schemas() -> Vec<(&'static str, Value)> {
    vec![
        ("schemas/models.schema.json", models_schema()),
        ("schemas/settings.schema.json", settings_schema()),
        ("schemas/keybindings.schema.json", keybindings_schema()),
        ("schemas/theme.schema.json", theme_schema()),
    ]
}
