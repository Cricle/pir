//! Per-provider compatibility auto-detection (upstream `detectCompat` +
//! `getCompat`, packages/ai/src/api/openai-completions.ts:1593-1734): uncataloged
//! models get the right wire shapes from the provider id / base URL; explicit
//! `model.compat` entries still win per field.

use crate::ai::types::{
    CacheControlFormat, Model, OpenAiCompat, SessionAffinityFormat, ThinkingFormat,
};

/// Auto-detect compatibility settings from provider name and base URL (upstream
/// `detectCompat`). Used as the base when `model.compat` is not set; explicit
/// `model.compat` entries override these detected values.
pub fn detect_compat(model: &Model) -> OpenAiCompat {
    let provider = model.provider.as_str();
    let base_url = model.base_url.as_str();

    let is_zai = provider == "zai"
        || provider == "zai-coding-cn"
        || base_url.contains("api.z.ai")
        || base_url.contains("open.bigmodel.cn");
    let is_together = provider == "together"
        || base_url.contains("api.together.ai")
        || base_url.contains("api.together.xyz");
    let is_moonshot = provider == "moonshotai"
        || provider == "moonshotai-cn"
        || base_url.contains("api.moonshot.");
    let is_openrouter = provider == "openrouter" || base_url.contains("openrouter.ai");
    let is_cloudflare_workers_ai =
        provider == "cloudflare-workers-ai" || base_url.contains("api.cloudflare.com");
    let is_cloudflare_ai_gateway =
        provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base_url.contains("api.ant-ling.com");
    let is_cerebras = provider == "cerebras" || base_url.contains("cerebras.ai");
    let is_deepseek = provider == "deepseek" || base_url.to_lowercase().contains("deepseek.com");

    let is_non_standard = is_nvidia
        || is_cerebras
        || provider == "xai"
        || base_url.contains("api.x.ai")
        || is_together
        || base_url.contains("chutes.ai")
        || is_deepseek
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || base_url.contains("opencode.ai")
        || is_cloudflare_workers_ai
        || is_cloudflare_ai_gateway
        || is_ant_ling;

    let use_max_tokens = base_url.contains("chutes.ai")
        || is_deepseek
        || is_moonshot
        || is_cloudflare_ai_gateway
        || is_together
        || is_nvidia
        || is_ant_ling
        || is_zai;

    let is_grok = provider == "xai" || base_url.contains("api.x.ai");
    let is_openrouter_developer_role_model =
        is_openrouter && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
    let cache_control_format = if provider == "openrouter" && model.id.starts_with("anthropic/") {
        Some(CacheControlFormat::Anthropic)
    } else {
        None
    };

    OpenAiCompat {
        supports_store: Some(!is_non_standard),
        supports_developer_role: Some(
            is_openrouter_developer_role_model || (!is_non_standard && !is_openrouter),
        ),
        supports_reasoning_effort: !is_grok
            && !is_zai
            && !is_moonshot
            && !is_together
            && !is_cloudflare_ai_gateway
            && !is_nvidia
            && !is_ant_ling,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: Some(
            if use_max_tokens {
                "max_tokens"
            } else {
                "max_completion_tokens"
            }
            .to_string(),
        ),
        requires_tool_result_name: Some(false),
        requires_assistant_after_tool_result: Some(false),
        requires_thinking_as_text: Some(false),
        requires_reasoning_content_on_assistant_messages: Some(is_deepseek),
        thinking_format: if is_deepseek {
            ThinkingFormat::Deepseek
        } else if is_zai {
            ThinkingFormat::Zai
        } else if is_together {
            ThinkingFormat::Together
        } else if is_ant_ling {
            ThinkingFormat::AntLing
        } else if is_openrouter {
            ThinkingFormat::Openrouter
        } else {
            ThinkingFormat::Openai
        },
        chat_template_kwargs: None,
        chat_template_args: None,
        zai_tool_stream: Some(false),
        supports_thinking_token_budget: Some(false),
        thinking_token_budget_field_compat: None,
        // OpenAI compatibility alone does not imply strict JSON-schema tool
        // support.
        supports_strict_mode: Some(false),
        supports_openai_grammar_tools: Some(false),
        supports_mid_convo_system_messages: Some(false),
        supports_mid_convo_tool_changes: Some(false),
        cache_control_format,
        send_session_affinity_headers: is_openrouter,
        session_affinity_format: Some(if is_openrouter {
            SessionAffinityFormat::Openrouter
        } else {
            SessionAffinityFormat::Openai
        }),
        supports_long_cache_retention: Some(
            !is_together
                && !is_cloudflare_workers_ai
                && !is_cloudflare_ai_gateway
                && !is_nvidia
                && !is_ant_ling,
        ),
        ..OpenAiCompat::default()
    }
}

/// Get resolved compatibility settings for a model (upstream `getCompat`):
/// auto-detect from provider/URL, then override with the explicit
/// `model.compat` entries. Boolean fields whose detection disagrees with the
/// serde default use a "differs → explicit wins" rule (pir's compat booleans
/// are not tri-state), so e.g. `supportsReasoningEffort: true` on an xAI-style
/// provider still overrides the detected `false`.
pub fn resolve_compat(model: &Model) -> OpenAiCompat {
    let mut resolved = detect_compat(model);
    let Some(explicit) = &model.compat else {
        return resolved;
    };
    if resolved.supports_reasoning_effort != explicit.supports_reasoning_effort {
        resolved.supports_reasoning_effort = explicit.supports_reasoning_effort;
    }
    if resolved.send_session_affinity_headers != explicit.send_session_affinity_headers {
        resolved.send_session_affinity_headers = explicit.send_session_affinity_headers;
    }
    // Everything else keeps the regular merge semantics: Option fields win
    // when Some, defaults-valued fields fall through to detection.
    resolved.merged_with(Some(explicit))
}
