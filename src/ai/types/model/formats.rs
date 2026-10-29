/// The cache-control marker convention an OpenAI-compatible provider accepts
/// (upstream `OpenAICompletionsCompat.cacheControlFormat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CacheControlFormat {
    /// Anthropic-style `cache_control: {type: "ephemeral"}` markers on the
    /// system prompt, the last tool definition, and the last user/assistant/
    /// tool-result text content (upstream `"anthropic"`).
    #[serde(rename = "anthropic")]
    Anthropic,
}

/// Session-affinity header format (upstream
/// `OpenAICompletionsCompat.sessionAffinityFormat`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SessionAffinityFormat {
    /// `session_id` + `x-client-request-id` + `x-session-affinity`.
    #[default]
    #[serde(rename = "openai")]
    Openai,
    /// `x-client-request-id` + `x-session-affinity` only.
    #[serde(rename = "openai-nosession")]
    OpenaiNosession,
    /// `x-session-id` only (OpenRouter).
    #[serde(rename = "openrouter")]
    Openrouter,
}

/// How a provider wants the thinking level expressed on the wire
/// (upstream `OpenAICompletionsCompat.thinkingFormat`).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum ThinkingFormat {
    /// Top-level `reasoning_effort` string (OpenAI, Together, Groq, ...).
    #[default]
    #[serde(rename = "openai")]
    Openai,
    /// Nested `reasoning: { effort }` object (OpenRouter normalization).
    #[serde(rename = "openrouter")]
    Openrouter,
    /// `reasoning: { enabled }` plus top-level `reasoning_effort`.
    #[serde(rename = "together")]
    Together,
    /// `chat_template_args` plus top-level `reasoning_effort` (Baseten).
    #[serde(rename = "baseten")]
    Baseten,
    /// `thinking: { type: enabled|disabled }` plus `reasoning_effort` (DeepSeek).
    #[serde(rename = "deepseek")]
    Deepseek,
    /// `thinking: { type: enabled|disabled, clear_thinking }` (Z.ai).
    #[serde(rename = "zai")]
    Zai,
    /// Top-level `enable_thinking` boolean plus `reasoning_effort` (Qwen).
    #[serde(rename = "qwen")]
    Qwen,
    /// `chat_template_kwargs` template arguments (vLLM-style).
    #[serde(rename = "chat-template")]
    ChatTemplate,
    /// `chat_template_kwargs: { enable_thinking, preserve_thinking }` (Qwen templates).
    #[serde(rename = "qwen-chat-template")]
    QwenChatTemplate,
    /// Top-level `thinking` string (llama.cpp / gpt-oss style).
    #[serde(rename = "string-thinking")]
    StringThinking,
    /// Nested `reasoning: { effort }` with explicit level-map entries only (AntLing).
    #[serde(rename = "ant-ling")]
    AntLing,
}
