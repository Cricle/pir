//! OpenAI-Responses compatibility knobs (upstream `OpenAIResponsesCompat`,
//! subset relevant to pir; the shared knobs that do exist on pir's
//! `OpenAiCompat` are read from there, everything else keeps the upstream
//! default).

use crate::ai::types::{Model, OpenAiCompat, SessionAffinityFormat};

/// Resolved Responses compatibility flags for one model (upstream
/// `getCompat`'s `Required<OpenAIResponsesCompat>`).
#[derive(Debug, Clone)]
pub struct ResponsesCompat {
    pub supports_developer_role: bool,
    pub supports_mid_convo_system_messages: bool,
    /// Whether thinking level changes are sent as `configuration_update`
    /// input items instead of changing `reasoning.effort`, so a change keeps
    /// the prompt cache (upstream `supportsMidConvoEffort`).
    pub supports_mid_convo_effort: bool,
    /// Session-affinity header format (upstream
    /// `OpenAIResponsesCompat.sessionAffinityFormat`; auto-detected from the
    /// provider/base URL when the model compat leaves it unset).
    pub session_affinity_format: SessionAffinityFormat,
    pub supports_long_cache_retention: bool,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub supports_additional_tools: bool,
    pub supports_tool_search: bool,
    /// Whether the model accepts `prompt_cache_options` (upstream
    /// `supportsExplicitPromptCacheMode`; older OpenAI models reject it).
    pub supports_explicit_prompt_cache_mode: bool,
    pub supports_max_output_tokens: bool,
    /// Whether the variant accepts `prompt_cache_retention` (upstream Azure
    /// `buildParams` sends only `prompt_cache_key`).
    pub supports_prompt_cache_retention: bool,
}

impl Default for ResponsesCompat {
    fn default() -> Self {
        Self {
            supports_developer_role: true,
            supports_mid_convo_system_messages: false,
            supports_mid_convo_effort: false,
            session_affinity_format: SessionAffinityFormat::Openai,
            supports_long_cache_retention: true,
            supports_strict_mode: false,
            supports_openai_grammar_tools: false,
            supports_additional_tools: false,
            supports_tool_search: false,
            supports_explicit_prompt_cache_mode: false,
            supports_max_output_tokens: true,
            supports_prompt_cache_retention: true,
        }
    }
}

/// Upstream `detectSessionAffinityFormat`: OpenRouter (by provider id or base
/// URL) pins requests with `x-session-id`, everyone else uses the OpenAI
/// format.
pub fn detect_session_affinity_format(model: &Model) -> SessionAffinityFormat {
    if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
        SessionAffinityFormat::Openrouter
    } else {
        SessionAffinityFormat::Openai
    }
}

impl ResponsesCompat {
    /// Resolve from a model's pir compat knobs (upstream `getCompat`): an
    /// explicitly set `model.compat` field wins; unset fields fall back to the
    /// upstream default (and `sessionAffinityFormat` to the auto-detection).
    pub fn from_model(model: &Model) -> Self {
        let default = OpenAiCompat::default();
        let compat = model.compat.as_ref().unwrap_or(&default);
        Self {
            supports_developer_role: compat.supports_developer_role.unwrap_or(true),
            supports_mid_convo_system_messages: compat
                .supports_mid_convo_system_messages
                .unwrap_or(false),
            supports_mid_convo_effort: compat.supports_mid_convo_effort.unwrap_or(false),
            session_affinity_format: compat
                .session_affinity_format
                .unwrap_or_else(|| detect_session_affinity_format(model)),
            supports_long_cache_retention: compat.supports_long_cache_retention.unwrap_or(true),
            supports_strict_mode: compat.supports_strict_mode.unwrap_or(false),
            supports_openai_grammar_tools: compat.supports_openai_grammar_tools.unwrap_or(false),
            supports_additional_tools: compat.supports_additional_tools.unwrap_or(false),
            supports_tool_search: compat.supports_tool_search.unwrap_or(false),
            supports_explicit_prompt_cache_mode: compat
                .supports_explicit_prompt_cache_mode
                .unwrap_or(false),
            supports_max_output_tokens: compat.supports_max_output_tokens.unwrap_or(true),
            supports_prompt_cache_retention: true,
        }
    }
}
