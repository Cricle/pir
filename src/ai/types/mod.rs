//! Core AI types: Message, Model, Usage, StopReason, Context, Tool, etc.
//!
//! These are the Rust equivalents of the upstream `packages/ai/src/types.ts`.
//! Rust idioms are used instead of mechanical TypeScript generics.

mod auth;
mod identity;
mod image;
mod messages;
mod model;
mod stream;
mod transcript;

/// Unique identifier for a provider.
pub type ProviderId = String;

/// Unique identifier for an API.
pub type ApiId = String;

/// Value types accepted in JSON-like tool arguments / metadata.
pub type JsonValue = serde_json::Value;

pub use auth::{
    ApiKeyCredential, AuthCheck, AuthResult, AuthType, Credential, CredentialType, ModelAuth,
};
pub use identity::{
    attribution_headers, client_version, format_user_agent, identity_headers, pi_attribution,
    ClientAttribution, ClientIdentity, UserAgentMode, PI_AI_VERSION,
};
pub use image::{
    AnyModel, AssistantImages, ClassifierAnswer, ClassifierApi, ClassifierBoolCriteria,
    ClassifierContext, ClassifierModel, ClassifierOptions, ClassifierQuestion,
    ClassifierRequestOptions, ClassifierResult, ClassifierStopReason, ImageApi, ImageModel,
    ImagesContent, ImagesContext, ImagesOptions, ImagesRequestOptions, ImagesStopReason,
    InputModality, ModelKind, OutputModality,
};
pub use messages::{
    AssistantMessage, AssistantRole, ConstrainedSamplingConfig, ContentBlock, Context,
    DeferredHandle, DiagnosticInfo, ImageContent, Message, NestedToolCallRecord,
    NestedToolCallStatus, NestedToolCalls, StrictPreference, TextContent, ThinkingContent, Tool,
    ToolCall, ToolReference, ToolResultMessage, ToolResultRole, UserMessage, UserRole,
};
pub use model::{
    calculate_cost, calculate_cost_with, clamp_thinking_level, get_supported_thinking_levels,
    thinking_level_supported, CacheControlFormat, Model, ModelCost, ModelCostRates, ModelCostTier,
    OpenAiCompat, SessionAffinityFormat, ThinkingFormat, ThinkingLevel, ThinkingTokenBudgetField,
    EXTENDED_THINKING_LEVELS,
};
pub use stream::{
    finalize_headers, has_header, has_non_empty_header, header_value, insert_header, merge_headers,
    suppress_header, AbortSignal, AbortedError, AssistantMessageEvent, AutoRetryEnd,
    AutoRetryEndHook, AutoRetryStart, AutoRetryStartHook, AzureRequestOptions,
    BedrockRequestOptions, CacheRetention, Cost, ErrorReason, FetchTransport, HeaderTransform,
    HeaderTransformHook, ProviderEnv, ProviderHeaders, ProviderStreamHooks, ReasoningSummary,
    RetryHook, RetryNotice, RetryOutcome, RetryWaitHook, SimpleStreamOptions, StopReason,
    StreamHook, StreamHookMut, StreamOptions, TerminalReason, TextVerbosity, ThinkingBudgets,
    ThinkingDisplay, ToolChoice, Usage,
};
pub use transcript::{
    collapse_system_messages, content_text, create_initial_system_message,
    get_current_system_message, get_current_system_prompt, get_current_tools,
    get_initial_system_message, is_system_message, normalize_context, resolve_transcript,
    system_message_text, without_initial_system_message, TranscriptContext,
};
