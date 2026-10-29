use super::*;

/// Per-provider/model OpenAI-Completions compatibility knobs
/// (upstream `OpenAICompletionsCompat`, subset relevant to pir).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OpenAiCompat {
    /// Whether the endpoint accepts a `reasoning_effort`-style field.
    #[serde(default = "default_true", rename = "supportsReasoningEffort")]
    pub supports_reasoning_effort: bool,
    /// Whether the provider supports the `strict` field in tool definitions
    /// (upstream `supportsStrictMode`; default `false` — OpenAI compatibility
    /// alone does not imply strict JSON-schema tool support).
    #[serde(
        default,
        rename = "supportsStrictMode",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_strict_mode: Option<bool>,
    /// Sampling parameter overrides selected by the effective pi thinking level
    /// (upstream `Model.samplingParamsByThinkingLevel`, keyed by thinking level
    /// wire name). Merged after [`Model::sampling_params`] and before
    /// request-level params.
    #[serde(
        default,
        rename = "samplingParamsByThinkingLevel",
        skip_serializing_if = "Option::is_none"
    )]
    pub sampling_params_by_thinking_level: Option<BTreeMap<String, BTreeMap<String, JsonValue>>>,
    /// Whether the provider supports Anthropic strict tool schemas
    /// (upstream `AnthropicMessagesCompat.supportsStrictTools`; default false).
    #[serde(
        default,
        rename = "supportsStrictTools",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_strict_tools: Option<bool>,
    /// Force adaptive thinking: `thinking.type: "adaptive"` plus
    /// `output_config.effort` regardless of model id
    /// (upstream `AnthropicMessagesCompat.forceAdaptiveThinking`).
    #[serde(
        default,
        rename = "forceAdaptiveThinking",
        skip_serializing_if = "Option::is_none"
    )]
    pub force_adaptive_thinking: Option<bool>,
    /// Replay empty thinking signatures instead of converting thinking to text
    /// (upstream `AnthropicMessagesCompat.allowEmptySignature`).
    #[serde(
        default,
        rename = "allowEmptySignature",
        skip_serializing_if = "Option::is_none"
    )]
    pub allow_empty_signature: Option<bool>,
    /// Whether the model accepts the Anthropic `temperature` request field
    /// (upstream `AnthropicMessagesCompat.supportsTemperature`; default true).
    #[serde(
        default,
        rename = "supportsTemperature",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_temperature: Option<bool>,
    /// Whether the provider accepts per-tool `eager_input_streaming`
    /// (upstream `AnthropicMessagesCompat.supportsEagerToolInputStreaming`;
    /// default true).
    #[serde(
        default,
        rename = "supportsEagerToolInputStreaming",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_eager_tool_input_streaming: Option<bool>,
    /// Whether the provider accepts Anthropic-style `cache_control` markers on
    /// tool definitions (upstream `AnthropicMessagesCompat.
    /// supportsCacheControlOnTools`; default true).
    #[serde(
        default,
        rename = "supportsCacheControlOnTools",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_cache_control_on_tools: Option<bool>,
    /// Whether the exact model accepts system-role messages after the
    /// conversation has started (upstream `AnthropicMessagesCompat.
    /// supportsMidConvoSystemMessages`).
    #[serde(
        default,
        rename = "supportsMidConvoSystemMessages",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_mid_convo_system_messages: Option<bool>,
    /// Whether system messages can introduce additional tools mid-conversation
    /// (upstream `OpenAICompletionsCompat.supportsMidConvoToolAdditions`;
    /// requires `supportsMidConvoSystemMessages`; default false).
    #[serde(
        default,
        rename = "supportsMidConvoToolAdditions",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_mid_convo_tool_additions: Option<bool>,
    /// Whether the exact model accepts mid-conversation `tool_addition` /
    /// `tool_removal` blocks (upstream
    /// `AnthropicMessagesCompat.supportsMidConvoToolChanges`).
    #[serde(
        default,
        rename = "supportsMidConvoToolChanges",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_mid_convo_tool_changes: Option<bool>,
    /// Whether the exact model transport supports effort-only system messages
    /// and thinking binding controls (upstream
    /// `AnthropicMessagesCompat.supportsMidConvoEffort`).
    #[serde(
        default,
        rename = "supportsMidConvoEffort",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_mid_convo_effort: Option<bool>,
    /// Whether `stream_options: { include_usage }` may be sent.
    #[serde(default = "default_true", rename = "supportsUsageInStreaming")]
    pub supports_usage_in_streaming: bool,
    /// Wire shape used to express the thinking level.
    #[serde(default, rename = "thinkingFormat")]
    pub thinking_format: ThinkingFormat,
    /// Field used for the response token cap (`max_tokens` for endpoints that
    /// reject the newer `max_completion_tokens`).
    #[serde(
        default,
        rename = "maxTokensField",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_tokens_field: Option<String>,
    /// Whether the provider reliably terminates streams with a terminal
    /// `finish_reason` (upstream `OpenAICompletionsCompat.
    /// supportsFinishReason`; default true — streams that end without one
    /// fail instead of guessing).
    #[serde(default = "default_true", rename = "supportsFinishReason")]
    pub supports_finish_reason: bool,
    /// Models the provider may transparently fall back to (upstream
    /// `AnthropicMessagesCompat.allowedFallbackModels`); non-empty enables the
    /// `server-side-fallback` Anthropic beta.
    #[serde(
        default,
        rename = "allowedFallbackModels",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_fallback_models: Option<Vec<serde_json::Value>>,
    /// Send session-affinity headers (upstream defaults this on for OpenRouter).
    #[serde(default, rename = "sendSessionAffinityHeaders")]
    pub send_session_affinity_headers: bool,
    /// Extra static headers appended to provider requests.
    #[serde(
        default,
        rename = "customHeaders",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub custom_headers: BTreeMap<String, String>,
    // --- OpenAI-completions knobs added for parity (upstream
    // `OpenAICompletionsCompat`, packages/ai/src/providers/compat-schema.ts).
    // All optional so explicit catalog/models.json entries keep winning over
    // the URL-based auto-detection (`api::openai::detect_compat`).
    /// Whether the provider supports the `store` field; when set, requests
    /// send `store: false` (upstream `supportsStore`).
    #[serde(
        default,
        rename = "supportsStore",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_store: Option<bool>,
    /// Whether the provider accepts the `developer` role for instruction
    /// messages on reasoning models (upstream `supportsDeveloperRole`).
    #[serde(
        default,
        rename = "supportsDeveloperRole",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_developer_role: Option<bool>,
    /// Whether tool results require the `name` field (upstream
    /// `requiresToolResultName`).
    #[serde(
        default,
        rename = "requiresToolResultName",
        skip_serializing_if = "Option::is_none"
    )]
    pub requires_tool_result_name: Option<bool>,
    /// Whether a user message after tool results requires an assistant
    /// message in between (upstream `requiresAssistantAfterToolResult`).
    #[serde(
        default,
        rename = "requiresAssistantAfterToolResult",
        skip_serializing_if = "Option::is_none"
    )]
    pub requires_assistant_after_tool_result: Option<bool>,
    /// Whether thinking blocks must be replayed as plain text (upstream
    /// `requiresThinkingAsText`).
    #[serde(
        default,
        rename = "requiresThinkingAsText",
        skip_serializing_if = "Option::is_none"
    )]
    pub requires_thinking_as_text: Option<bool>,
    /// Whether every replayed assistant message must carry an empty
    /// `reasoning_content` field when reasoning is enabled (DeepSeek; upstream
    /// `requiresReasoningContentOnAssistantMessages`).
    #[serde(
        default,
        rename = "requiresReasoningContentOnAssistantMessages",
        skip_serializing_if = "Option::is_none"
    )]
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    /// Whether z.ai accepts top-level `tool_stream` for streaming tool-call
    /// deltas (upstream `zaiToolStream`).
    #[serde(
        default,
        rename = "zaiToolStream",
        skip_serializing_if = "Option::is_none"
    )]
    pub zai_tool_stream: Option<bool>,
    /// Whether the provider accepts OpenAI custom tools with Lark or regex
    /// grammar formats (upstream `supportsOpenAIGrammarTools`).
    #[serde(
        default,
        rename = "supportsOpenAIGrammarTools",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_openai_grammar_tools: Option<bool>,
    /// Whether the model supports message-anchored `additional_tools` input
    /// items (upstream `OpenAIResponsesCompat.supportsAdditionalTools`;
    /// default false).
    #[serde(
        default,
        rename = "supportsAdditionalTools",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_additional_tools: Option<bool>,
    /// Whether the model supports client-executed tool search for
    /// transcript-anchored additions (upstream
    /// `OpenAIResponsesCompat.supportsToolSearch`; default false).
    #[serde(
        default,
        rename = "supportsToolSearch",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_tool_search: Option<bool>,
    /// Whether the model accepts `prompt_cache_options`; older OpenAI models
    /// reject the parameter (upstream
    /// `OpenAIResponsesCompat.supportsExplicitPromptCacheMode`; default false).
    #[serde(
        default,
        rename = "supportsExplicitPromptCacheMode",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_explicit_prompt_cache_mode: Option<bool>,
    /// Whether the provider accepts `max_output_tokens`; some Codex-protocol
    /// gateways reject it (upstream
    /// `OpenAIResponsesCompat.supportsMaxOutputTokens`; default true).
    #[serde(
        default,
        rename = "supportsMaxOutputTokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_max_output_tokens: Option<bool>,
    /// Cache-control marker convention (upstream `cacheControlFormat`).
    #[serde(
        default,
        rename = "cacheControlFormat",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_control_format: Option<CacheControlFormat>,
    /// Session-affinity header format (upstream `sessionAffinityFormat`).
    #[serde(
        default,
        rename = "sessionAffinityFormat",
        skip_serializing_if = "Option::is_none"
    )]
    pub session_affinity_format: Option<SessionAffinityFormat>,
    /// Whether the provider supports long prompt cache retention
    /// (`prompt_cache_retention: "24h"` / `cache_control.ttl: "1h"`;
    /// upstream `supportsLongCacheRetention`).
    #[serde(
        default,
        rename = "supportsLongCacheRetention",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_long_cache_retention: Option<bool>,
    /// OpenRouter provider routing preferences sent as the top-level
    /// `provider` field (upstream `openRouterRouting`).
    #[serde(
        default,
        rename = "openRouterRouting",
        skip_serializing_if = "Option::is_none"
    )]
    pub open_router_routing: Option<JsonValue>,
    /// Vercel AI Gateway routing preferences sent as
    /// `providerOptions.gateway` (upstream `vercelGatewayRouting`).
    #[serde(
        default,
        rename = "vercelGatewayRouting",
        skip_serializing_if = "Option::is_none"
    )]
    pub vercel_gateway_routing: Option<JsonValue>,
    /// Template kwargs sent as `chat_template_kwargs` when the thinking
    /// format is `chat-template` (upstream `chatTemplateKwargs`). Values may
    /// be pi-controlled `{ $var: "thinking.enabled" | "thinking.budget",
    /// omitWhenOff? }` objects.
    #[serde(
        default,
        rename = "chatTemplateKwargs",
        skip_serializing_if = "Option::is_none"
    )]
    pub chat_template_kwargs: Option<BTreeMap<String, JsonValue>>,
    /// Template arguments sent as `chat_template_args` when the thinking
    /// format is `baseten` (upstream `chatTemplateArgs`).
    #[serde(
        default,
        rename = "chatTemplateArgs",
        skip_serializing_if = "Option::is_none"
    )]
    pub chat_template_args: Option<BTreeMap<String, JsonValue>>,
    /// vLLM scheduler priority sent as the top-level `priority` field
    /// (upstream `vllmPriority`).
    #[serde(
        default,
        rename = "vllmPriority",
        skip_serializing_if = "Option::is_none"
    )]
    pub vllm_priority: Option<f64>,
    /// Alias for `thinkingTokenBudgetField: "thinking_token_budget"`
    /// (upstream `supportsThinkingTokenBudget`).
    #[serde(
        default,
        rename = "supportsThinkingTokenBudget",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_thinking_token_budget: Option<bool>,
    /// Top-level request field used to cap reasoning tokens from
    /// `thinkingBudgets` (upstream `thinkingTokenBudgetField`; vLLM / Qwen /
    /// llama.cpp).
    #[serde(
        default,
        rename = "thinkingTokenBudgetField",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_token_budget_field_compat: Option<ThinkingTokenBudgetField>,
    /// Input size limits per input type (upstream `Model.inputLimits`,
    /// e.g. `{"images": {"maxImageSize": ..., "resize": ...}}`). pir carries
    /// it on the compat block (like `samplingParamsByThinkingLevel`) so
    /// models.json definitions round-trip without touching `Model`'s shape.
    #[serde(
        default,
        rename = "inputLimits",
        skip_serializing_if = "Option::is_none"
    )]
    pub input_limits: Option<JsonValue>,
    /// Prompt-cache configuration (upstream `Model.promptCache`). Carried on
    /// the compat block like `inputLimits`; metadata only in pir's adapters.
    #[serde(
        default,
        rename = "promptCache",
        skip_serializing_if = "Option::is_none"
    )]
    pub prompt_cache: Option<JsonValue>,
    /// The api key is sent as `Authorization: Bearer <apiKey>` instead of the
    /// provider's default credential header (upstream provider-level
    /// `authHeader` from models.json; carried on the model because pir's
    /// request-header composition runs in the registry stream path).
    #[serde(
        default,
        rename = "authHeader",
        skip_serializing_if = "Option::is_none"
    )]
    pub auth_header: Option<bool>,
}

fn default_true() -> bool {
    true
}

impl Default for OpenAiCompat {
    fn default() -> Self {
        Self {
            supports_reasoning_effort: true,
            supports_strict_mode: None,
            sampling_params_by_thinking_level: None,
            supports_strict_tools: None,
            force_adaptive_thinking: None,
            allow_empty_signature: None,
            supports_temperature: None,
            supports_eager_tool_input_streaming: None,
            supports_cache_control_on_tools: None,
            supports_mid_convo_system_messages: None,
            supports_mid_convo_tool_additions: None,
            supports_mid_convo_tool_changes: None,
            supports_mid_convo_effort: None,
            supports_usage_in_streaming: true,
            thinking_format: ThinkingFormat::Openai,
            max_tokens_field: None,
            supports_finish_reason: true,
            allowed_fallback_models: None,
            send_session_affinity_headers: false,
            custom_headers: BTreeMap::new(),
            supports_store: None,
            supports_developer_role: None,
            requires_tool_result_name: None,
            requires_assistant_after_tool_result: None,
            requires_thinking_as_text: None,
            requires_reasoning_content_on_assistant_messages: None,
            zai_tool_stream: None,
            supports_openai_grammar_tools: None,
            supports_additional_tools: None,
            supports_tool_search: None,
            supports_explicit_prompt_cache_mode: None,
            supports_max_output_tokens: None,
            cache_control_format: None,
            session_affinity_format: None,
            supports_long_cache_retention: None,
            open_router_routing: None,
            vercel_gateway_routing: None,
            chat_template_kwargs: None,
            chat_template_args: None,
            vllm_priority: None,
            supports_thinking_token_budget: None,
            thinking_token_budget_field_compat: None,
            input_limits: None,
            prompt_cache: None,
            auth_header: None,
        }
    }
}

impl OpenAiCompat {
    /// Merge a model-level compat over this provider-level compat
    /// (upstream deep-merges: the model level wins per explicitly-set field,
    /// the provider level fills the rest). Fields left at their default on the
    /// model level are treated as unset.
    pub fn merged_with(&self, model_level: Option<&OpenAiCompat>) -> OpenAiCompat {
        let mut merged = self.clone();
        let Some(over) = model_level else {
            return merged;
        };
        let defaults = OpenAiCompat::default();
        if over.supports_reasoning_effort != defaults.supports_reasoning_effort {
            merged.supports_reasoning_effort = over.supports_reasoning_effort;
        }
        if over.supports_strict_mode.is_some() {
            merged.supports_strict_mode = over.supports_strict_mode;
        }
        if over.sampling_params_by_thinking_level.is_some() {
            merged.sampling_params_by_thinking_level =
                over.sampling_params_by_thinking_level.clone();
        }
        if over.supports_strict_tools.is_some() {
            merged.supports_strict_tools = over.supports_strict_tools;
        }
        if over.force_adaptive_thinking.is_some() {
            merged.force_adaptive_thinking = over.force_adaptive_thinking;
        }
        if over.allow_empty_signature.is_some() {
            merged.allow_empty_signature = over.allow_empty_signature;
        }
        if over.supports_temperature.is_some() {
            merged.supports_temperature = over.supports_temperature;
        }
        if over.supports_eager_tool_input_streaming.is_some() {
            merged.supports_eager_tool_input_streaming = over.supports_eager_tool_input_streaming;
        }
        if over.supports_cache_control_on_tools.is_some() {
            merged.supports_cache_control_on_tools = over.supports_cache_control_on_tools;
        }
        if over.supports_mid_convo_system_messages.is_some() {
            merged.supports_mid_convo_system_messages = over.supports_mid_convo_system_messages;
        }
        if over.supports_mid_convo_tool_additions.is_some() {
            merged.supports_mid_convo_tool_additions = over.supports_mid_convo_tool_additions;
        }
        if over.supports_mid_convo_tool_changes.is_some() {
            merged.supports_mid_convo_tool_changes = over.supports_mid_convo_tool_changes;
        }
        if over.supports_mid_convo_effort.is_some() {
            merged.supports_mid_convo_effort = over.supports_mid_convo_effort;
        }
        if over.supports_usage_in_streaming != defaults.supports_usage_in_streaming {
            merged.supports_usage_in_streaming = over.supports_usage_in_streaming;
        }
        if over.thinking_format != defaults.thinking_format {
            merged.thinking_format = over.thinking_format;
        }
        if over.send_session_affinity_headers != defaults.send_session_affinity_headers {
            merged.send_session_affinity_headers = over.send_session_affinity_headers;
        }
        if over.max_tokens_field.is_some() {
            merged.max_tokens_field = over.max_tokens_field.clone();
        }
        if over.supports_finish_reason != defaults.supports_finish_reason {
            merged.supports_finish_reason = over.supports_finish_reason;
        }
        if over.allowed_fallback_models.is_some() {
            merged.allowed_fallback_models = over.allowed_fallback_models.clone();
        }
        for (name, value) in &over.custom_headers {
            merged.custom_headers.insert(name.clone(), value.clone());
        }
        if over.supports_store.is_some() {
            merged.supports_store = over.supports_store;
        }
        if over.supports_developer_role.is_some() {
            merged.supports_developer_role = over.supports_developer_role;
        }
        if over.requires_tool_result_name.is_some() {
            merged.requires_tool_result_name = over.requires_tool_result_name;
        }
        if over.requires_assistant_after_tool_result.is_some() {
            merged.requires_assistant_after_tool_result = over.requires_assistant_after_tool_result;
        }
        if over.requires_thinking_as_text.is_some() {
            merged.requires_thinking_as_text = over.requires_thinking_as_text;
        }
        if over
            .requires_reasoning_content_on_assistant_messages
            .is_some()
        {
            merged.requires_reasoning_content_on_assistant_messages =
                over.requires_reasoning_content_on_assistant_messages;
        }
        if over.zai_tool_stream.is_some() {
            merged.zai_tool_stream = over.zai_tool_stream;
        }
        if over.supports_openai_grammar_tools.is_some() {
            merged.supports_openai_grammar_tools = over.supports_openai_grammar_tools;
        }
        if over.supports_additional_tools.is_some() {
            merged.supports_additional_tools = over.supports_additional_tools;
        }
        if over.supports_tool_search.is_some() {
            merged.supports_tool_search = over.supports_tool_search;
        }
        if over.supports_explicit_prompt_cache_mode.is_some() {
            merged.supports_explicit_prompt_cache_mode = over.supports_explicit_prompt_cache_mode;
        }
        if over.supports_max_output_tokens.is_some() {
            merged.supports_max_output_tokens = over.supports_max_output_tokens;
        }
        if over.cache_control_format.is_some() {
            merged.cache_control_format = over.cache_control_format;
        }
        if over.session_affinity_format.is_some() {
            merged.session_affinity_format = over.session_affinity_format;
        }
        if over.supports_long_cache_retention.is_some() {
            merged.supports_long_cache_retention = over.supports_long_cache_retention;
        }
        if over.open_router_routing.is_some() {
            merged.open_router_routing = over.open_router_routing.clone();
        }
        if over.vercel_gateway_routing.is_some() {
            merged.vercel_gateway_routing = over.vercel_gateway_routing.clone();
        }
        if over.chat_template_kwargs.is_some() {
            merged.chat_template_kwargs = over.chat_template_kwargs.clone();
        }
        if over.chat_template_args.is_some() {
            merged.chat_template_args = over.chat_template_args.clone();
        }
        if over.vllm_priority.is_some() {
            merged.vllm_priority = over.vllm_priority;
        }
        if over.supports_thinking_token_budget.is_some() {
            merged.supports_thinking_token_budget = over.supports_thinking_token_budget;
        }
        if over.thinking_token_budget_field_compat.is_some() {
            merged.thinking_token_budget_field_compat = over.thinking_token_budget_field_compat;
        }
        if over.input_limits.is_some() {
            merged.input_limits = over.input_limits.clone();
        }
        if over.prompt_cache.is_some() {
            merged.prompt_cache = over.prompt_cache.clone();
        }
        if over.auth_header.is_some() {
            merged.auth_header = over.auth_header;
        }
        merged
    }
}
