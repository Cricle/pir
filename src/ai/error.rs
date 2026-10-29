//! AI-specific error types.
//!
//! These are lower-level errors for the AI module, distinct from the
//! top-level `pir::Error`. They are converted to `AssistantMessage` with
//! `stop_reason: Error` by provider implementations.

use thiserror::Error;

use crate::ai::types::{AssistantMessage, DiagnosticInfo, StopReason};

/// AI module result type.
pub type AiResult<T> = Result<T, AiError>;

/// Errors produced by the AI module.
#[derive(Debug, Error)]
pub enum AiError {
    /// The stream ended without a terminal event.
    #[error("stream ended without a terminal event")]
    StreamNotTerminated,

    /// The stream ended (a clean EOF after a successful response) before its
    /// provider terminal event: no finish reason, no stop reason, no
    /// `message_stop`, no `response.completed`. The transport rendered no
    /// verdict (it read a complete body), so this is the parser-visible
    /// truncation class — [retryable](TurnRetryClass::RetryableTurn) at the
    /// agent level, exactly like [`Self::StreamNotTerminated`]. The payload is
    /// the adapter's own wording, kept because tests and logs pin it.
    #[error("{0}")]
    StreamTruncated(String),

    /// The provider is not configured.
    #[error("provider not configured: {0}")]
    ProviderNotConfigured(String),

    /// Unknown provider id.
    #[error("unknown provider: {0}")]
    UnknownProvider(String),

    /// Unknown model id.
    #[error("unknown model: {0}")]
    UnknownModel(String),

    /// The model's wire api has no adapter in this build: the provider family
    /// was compiled out (`provider-…` feature disabled). Never a silent
    /// fallback — construction reports the missing feature by name.
    #[error(
        "the '{api}' API adapter is not available in this build of pir: it was compiled \
         without the `{feature}` feature"
    )]
    MissingProviderAdapter { api: String, feature: &'static str },

    /// Authentication failure.
    #[error("authentication failure: {0}")]
    Authentication(String),

    /// A `models-store.json` persistence failure: the store file could not be
    /// read, parsed, serialized, or atomically replaced. Distinct from
    /// [`Self::Authentication`] so a failed catalog write is never reported as
    /// an authentication problem.
    #[error("models store error: {0}")]
    ModelsStore(String),

    /// A retry-exhausted error.
    #[error("retry exhausted after {attempts} attempt(s): {message}")]
    RetryExhausted { attempts: u32, message: String },

    /// Aborted by the caller.
    #[error("request aborted")]
    Aborted,

    /// The user cancelled a scheduled retry (upstream `abortRetry()` →
    /// `auto_retry_end { success: false, finalError: "Retry cancelled" }`,
    /// rendered as `Retry failed after {attempt} attempts: Retry cancelled`).
    /// `attempts` is the cancelled retry's 1-based ordinal.
    #[error("Retry failed after {attempts} attempts: Retry cancelled")]
    RetryCancelled { attempts: u32 },

    /// A provider returned an error that should not be retried.
    #[error("non-retryable provider error: {0}")]
    NonRetryable(String),

    /// A provider-authored error frame that is transient (HTTP 429/5xx,
    /// `overloaded`, rate limited, resource exhausted, throttled, server
    /// busy). The text is the provider's own message, unchanged, so callers
    /// that display it keep the adapter wording; the retry disposition is
    /// carried by the variant (see [`TurnRetryClass::RetryableTurn`]).
    #[error("{0}")]
    Retryable(String),

    /// An I/O error wrapping the source.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON error wrapping the source.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// An HTTP transport error.
    #[error("HTTP error: {0}")]
    Http(String),

    /// The response body exceeded the configured limit.
    #[error("response body exceeds {limit} bytes")]
    ResponseTooLarge { limit: usize },
}

/// The HTTP status and response body carried by a transport failure.
///
/// The shared transport reports every non-2xx response as
/// [`AiError::NonRetryable`] with the stable text
/// `"HTTP <status>: <body>"` (see [`crate::ai::transport`]). The text shape is
/// kept for every existing caller (adapter parsers, `Display`, tests), and this
/// accessor exposes the same values structurally so finalizers can rebuild the
/// upstream-shaped user-facing message (`<prefix> (<status>): <body>` or
/// `<status>: <body>`, upstream `formatProviderError`, error-body.ts:130-137)
/// without re-sniffing strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderHttpError<'a> {
    /// The HTTP status code.
    pub status: u16,
    /// The (already truncated) response body.
    pub body: &'a str,
}

/// Structural retry disposition of a terminal provider failure, decided while
/// the failure is still a typed [`AiError`] and carried to the agent loop on
/// the terminal [`AssistantMessage`].
///
/// The agent-level turn retry must re-issue ONLY [`RetryableTurn`]. The text
/// of the error message is not a reliable selector: adapter prefixes
/// ("OpenAI API error: …", "Azure OpenAI API error: …") hide the transport
/// verdict, and an exhausted 429/503 body still contains retryable words
/// ("429", "503", "overloaded"). The variant carries the verdict instead; the
/// class travels with the message through an internal diagnostic entry that
/// the agent loop takes before the message enters any event, history, or
/// transcript (see [`attach_turn_retry_class`] / [`take_turn_retry_class`]).
///
/// [`RetryableTurn`]: TurnRetryClass::RetryableTurn
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnRetryClass {
    /// The failure never reached a transport-level retry verdict (a
    /// mid-stream drop, body read failure, or unterminated stream). The agent
    /// loop may re-issue the whole turn after its own backoff.
    RetryableTurn,
    /// The transport already spent its own retry budget
    /// (`AiError::RetryExhausted`, including a zero budget). Re-issuing at the
    /// agent level would multiply the two budgets into extra requests.
    TransportExhausted,
    /// A provider/transport verdict that must never be re-issued
    /// (`AiError::NonRetryable`, authentication/configuration failures,
    /// aborts, response-size limits, and parse failures).
    NonRetryable,
}

impl TurnRetryClass {
    /// Stable machine-readable spelling used by the internal diagnostic
    /// carrier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RetryableTurn => "retryable_turn",
            Self::TransportExhausted => "transport_exhausted",
            Self::NonRetryable => "non_retryable",
        }
    }

    /// Parse the spelling written by [`Self::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "retryable_turn" => Some(Self::RetryableTurn),
            "transport_exhausted" => Some(Self::TransportExhausted),
            "non_retryable" => Some(Self::NonRetryable),
            _ => None,
        }
    }
}

/// Diagnostic `type` marking the internal retry-class carrier appended to a
/// terminal error message by [`attach_turn_retry_class`].
///
/// This entry is consumed and REMOVED by the agent loop (or by a caller that
/// reads [`take_turn_retry_class`] / [`strip_turn_retry_class`]) before the
/// message enters any transcript, so it is never persisted and never reaches a
/// provider request. The removal covers EVERY copy: a foreign `streamSimple`
/// provider hands its events over whole and can plant or duplicate the entry.
pub const RETRY_CLASS_DIAGNOSTIC: &str = "pir_retry_class";

impl AiError {
    /// Structural retry disposition of this error (see [`TurnRetryClass`]).
    ///
    /// Derived from the VARIANT only — never from [`Self::user_message`] or
    /// `Display`, whose text adapters prefix and which [`AiError::RetryExhausted`]
    /// flattens to its inner message. `Io`/`Http`/`StreamNotTerminated`/
    /// `StreamTruncated` are the errors the transport surfaces when a stream
    /// dies after it started (the transport never rendered a verdict for
    /// those, so the agent's turn retry is the outer budget).
    pub fn turn_retry_class(&self) -> TurnRetryClass {
        match self {
            AiError::RetryExhausted { .. } => TurnRetryClass::TransportExhausted,
            AiError::Aborted
            | AiError::RetryCancelled { .. }
            | AiError::NonRetryable(_)
            | AiError::ProviderNotConfigured(_)
            | AiError::UnknownProvider(_)
            | AiError::UnknownModel(_)
            | AiError::MissingProviderAdapter { .. }
            | AiError::Authentication(_)
            | AiError::ModelsStore(_)
            | AiError::Json(_)
            | AiError::ResponseTooLarge { .. } => TurnRetryClass::NonRetryable,
            AiError::StreamNotTerminated
            | AiError::StreamTruncated(_)
            | AiError::Retryable(_)
            | AiError::Io(_)
            | AiError::Http(_) => TurnRetryClass::RetryableTurn,
        }
    }

    /// The HTTP status and body of a transport failure, exposed structurally.
    ///
    /// Only the shared transport's non-2xx shape is recognized
    /// (`NonRetryable("HTTP <status>: <body>")`); every other `AiError` —
    /// including adapter-authored `NonRetryable` messages — returns `None`.
    /// The `Display`/text shape is unchanged for existing callers.
    pub fn provider_http_error(&self) -> Option<ProviderHttpError<'_>> {
        let AiError::NonRetryable(message) = self else {
            return None;
        };
        let rest = message.strip_prefix("HTTP ")?;
        let (status, body) = rest.split_once(": ")?;
        Some(ProviderHttpError {
            status: status.parse().ok()?,
            body,
        })
    }
}

/// Attach the structural retry disposition of `error` to the terminal
/// `message` the provider adapter is about to emit.
///
/// The carrier is an internal [`DiagnosticInfo`] (`type` =
/// [`RETRY_CLASS_DIAGNOSTIC`], `error.code` = [`TurnRetryClass::as_str`]).
/// It is additive metadata: the user-facing `error_message` text is unchanged.
/// Consumers that make a retry decision must use [`take_turn_retry_class`],
/// which removes the entry so it cannot be persisted.
pub fn attach_turn_retry_class(message: &mut AssistantMessage, error: &AiError) {
    message.diagnostics.push(DiagnosticInfo {
        diag_type: RETRY_CLASS_DIAGNOSTIC.to_string(),
        timestamp: crate::ai::util::now_ms(),
        error_message: None,
        error_name: None,
        error_stack: None,
        error_code: Some(serde_json::Value::String(
            error.turn_retry_class().as_str().to_string(),
        )),
        details: None,
    });
}

/// Remove EVERY [`RETRY_CLASS_DIAGNOSTIC`] carrier from `message` without
/// interpreting any of them, returning how many entries were removed.
///
/// Serialization boundaries that must be carrier-free but make no retry
/// decision (the extension `message_start` fallback, `CompleteOk`) use this
/// directly; consumers that need the verdict use [`take_turn_retry_class`].
/// Removal is the security-relevant half: a foreign/duplicated marker (a JS
/// `streamSimple` provider hands its events over whole, so it can plant or
/// duplicate the entry) must never survive into a transcript or a frame.
pub fn strip_turn_retry_class(message: &mut AssistantMessage) -> usize {
    let before = message.diagnostics.len();
    message
        .diagnostics
        .retain(|diagnostic| diagnostic.diag_type != RETRY_CLASS_DIAGNOSTIC);
    before - message.diagnostics.len()
}

/// Take (and REMOVE every copy of) the structural retry disposition attached
/// by [`attach_turn_retry_class`].
///
/// ALL carriers are removed, not just the first: a duplicated or foreign
/// `pir_retry_class` entry (see [`strip_turn_retry_class`]) must not survive
/// the take just because the genuine one came first.
///
/// The returned verdict is deterministic: the FIRST carrier in diagnostic
/// order decides, and it decides even when its `error.code` is missing or
/// unrecognized (the duplicate cannot "rescue" a malformed leading marker).
/// A malformed first carrier therefore yields `None` — the fail-closed
/// direction: the turn is not re-issued at agent level.
///
/// Returns `None` for a message produced outside the provider finalizers
/// (synthetic host/embedder messages), which callers must treat as
/// not-retryable at the agent level.
pub fn take_turn_retry_class(message: &mut AssistantMessage) -> Option<TurnRetryClass> {
    let verdict = message
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.diag_type == RETRY_CLASS_DIAGNOSTIC)
        .and_then(|diagnostic| {
            diagnostic
                .error_code
                .as_ref()
                .and_then(serde_json::Value::as_str)
                .and_then(TurnRetryClass::parse)
        });
    strip_turn_retry_class(message);
    verdict
}

impl AiError {
    /// Map this error to an `AssistantMessage` stop reason.
    pub fn stop_reason(&self) -> StopReason {
        match self {
            AiError::Aborted => StopReason::Aborted,
            _ => StopReason::Error,
        }
    }

    /// Returns the error message suitable for including in an `AssistantMessage`.
    pub fn user_message(&self) -> String {
        // Sensitive error truncation: avoid leaking internal paths, keys, etc.
        // For now, use the Display impl; real providers will redact further.
        match self {
            AiError::Aborted => "Request was aborted".to_string(),
            AiError::RetryExhausted { message, .. } => {
                // Truncate long error messages from retry chains. The cut must
                // land on a char boundary: provider errors are not guaranteed
                // ASCII, and slicing mid-codepoint would panic.
                if message.len() > 200 {
                    let mut end = 200;
                    while !message.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}...", &message[..end])
                } else {
                    message.clone()
                }
            }
            _ => self.to_string(),
        }
    }
}
