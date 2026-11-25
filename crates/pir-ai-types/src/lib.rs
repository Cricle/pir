//! Provider-neutral AI value types: `Message`, `Model`, `Usage`,
//! `StopReason`, `Context`, `Tool`, the stream options and the assistant event
//! stream.
//!
//! These are the Rust equivalents of the upstream `packages/ai/src/types.ts`.
//! The crate also carries the provider HTTP transport *seam* (the trait and its
//! request/response shapes, no implementation) and the AI error type, because
//! the stream options reference both. `pir-core` re-exports every module here
//! at the historical `crate::ai::{types,util,event_stream,error,transport}`
//! paths, so no existing call site changes.

mod auth;
mod identity;
mod image;
mod message_source;
mod messages;
mod model;
pub mod seam;
mod stream;
mod transcript;

pub mod error;
pub mod event_stream;
pub mod partial_mirror;
pub mod session;
pub mod text;
pub mod util;

pub use error::{is_retryable, AiError, AiResult, ProviderHttpError, TurnRetryClass};
pub use seam::{
    apply_after_provider_response, AfterProviderResponseHook, BeforeProviderRequestHook, ChunkSink,
    HttpBodyChunk, HttpBodyStream, HttpRequest, HttpResponse, HttpTransport,
};

/// A boxed `Send` future, the return type of every async seam in this crate.
///
/// The agent's tool execution uses the same alias; `pir-core` re-exports it so
/// both sides share one definition.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

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
pub use message_source::{
    approximate_message_json_bytes, message_has_tool_history, summarize_messages,
    ChainedMessageSource, MessageSource, MessageSourceSummary, SliceMessageSource, SourceMessage,
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
    OpenAiCompat, PromptCacheKeyMode, SessionAffinityFormat, ThinkingFormat, ThinkingLevel,
    ThinkingTokenBudgetField, EXTENDED_THINKING_LEVELS,
};
pub use stream::{
    finalize_headers, has_header, has_non_empty_header, header_value, insert_header, merge_headers,
    suppress_header, AbortSignal, AbortedError, AssistantMessageEvent, AutoRetryEnd,
    AutoRetryEndHook, AutoRetryStart, AutoRetryStartHook, AzureRequestOptions,
    BedrockRequestOptions, BeforeProviderHeadersHook, CacheRetention, Cost, ErrorReason,
    FetchTransport, HeaderTransform, HeaderTransformHook, ProviderEnv, ProviderHeaders,
    ProviderStreamHooks, ReasoningSummary, RetryHook, RetryNotice, RetryOutcome, RetryWaitHook,
    SimpleStreamOptions, StopReason, StreamHook, StreamHookMut, StreamOptions, TerminalReason,
    TextVerbosity, ThinkingBudgets, ThinkingDisplay, ToolChoice, Usage,
};
pub use transcript::{
    collapse_system_messages, content_text, create_initial_system_message,
    get_current_system_message, get_current_system_prompt, get_current_tools,
    get_initial_system_message, is_prompt_snapshot, is_system_message, leading_system_fragment,
    live_prompt_carries_section, normalize_context, resolve_transcript, system_message_text,
    without_initial_system_message, TranscriptContext,
};
