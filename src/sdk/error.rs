//! The SDK error type: a stable, typed boundary over the internal error
//! paths (no `anyhow` leakage across the public API surface).

/// Errors returned by the SDK facade.
///
/// Internal machinery keeps its own conventions (`anyhow`, [`AiError`]);
/// everything crossing into SDK consumers is classified into this enum.
/// The classification is total: [`SdkError::Other`] catches anything the
/// facade cannot classify more precisely.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SdkError {
    /// Model resolution failed: no endpoint was configured and neither
    /// `models.json`, the built-in catalog, nor environment credentials
    /// yielded an available model.
    #[error("no model available: {message}")]
    NoModel {
        /// Human-readable resolution failure detail (upstream guidance).
        message: String,
    },

    /// The AI layer rejected a request (unknown provider/model, auth
    /// failure, transport error, non-terminated stream, ...).
    #[error("provider error: {0}")]
    Provider(#[from] crate::ai::error::AiError),

    /// A filesystem or transport I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Session storage failed to open, create, or persist entries.
    #[error("session error: {0}")]
    Session(String),

    /// An extension/plugin surface failed at the SDK boundary: an on-disk
    /// extension path could not be loaded, or the JS extension runtime could
    /// not start during [`crate::sdk::AgentBuilder::build`]. Plugin
    /// `on_shutdown` hook failures are diagnosed (not returned) by
    /// [`crate::sdk::Agent::shutdown`].
    #[error("extension error: {0}")]
    Extension(String),

    /// The run was aborted before any assistant content was produced.
    /// Later aborts surface as [`crate::sdk::AgentOutcome::aborted`] on the
    /// successful outcome instead, so partial content is never dropped.
    #[error("operation aborted")]
    Aborted,

    /// A requested capability is not compiled into this build (for example a
    /// tool factory whose `tool-*` cargo feature is off) or is not supported
    /// by the current host. The message names the missing feature/capability.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// Any error the facade does not classify above (including internal
    /// `anyhow` errors from the session machinery).
    #[error("{0}")]
    Other(String),
}

impl SdkError {
    /// Classify an internal `anyhow` error at the SDK boundary:
    ///
    /// - AI-layer errors downcast to [`SdkError::Provider`],
    /// - I/O errors downcast to [`SdkError::Io`] preserving the original
    ///   error's own display text (anyhow context layers are not folded into
    ///   the message), including when wrapped in context (chain walk),
    /// - session-machinery failures that are neither of the above classify as
    ///   [`SdkError::Session`] (the [`crate::agent_session::AgentSession`]
    ///   run path only produces these for session storage, the streaming
    ///   guard aside, which is matched explicitly below),
    /// - anything else becomes [`SdkError::Other`].
    ///
    /// Owned AI-layer errors also enter through the direct
    /// [`From<crate::ai::error::AiError>`] conversion.
    pub(crate) fn from_anyhow(error: anyhow::Error) -> Self {
        Self::classify(error, false)
    }

    /// Classify an error surfaced by the [`crate::agent_session::AgentSession`]
    /// run boundary ([`crate::sdk::Agent::prompt`]): besides the AI/I/O
    /// downcasts, run-path failures that are neither AI nor I/O and not a
    /// precondition rejection (the streaming guard and the
    /// compaction-in-progress guard) are session-machinery failures (storage
    /// open/append/projection) and classify as [`SdkError::Session`].
    pub(crate) fn from_session_boundary(error: anyhow::Error) -> Self {
        Self::classify(error, true)
    }

    fn classify(error: anyhow::Error, boundary_session_errors: bool) -> Self {
        // `downcast` consumes the error; on failure it hands it back.
        let error = match error.downcast::<crate::ai::error::AiError>() {
            Ok(ai_error) => return Self::Provider(ai_error),
            Err(error) => error,
        };
        let error = match error.downcast::<std::io::Error>() {
            Ok(io_error) => return Self::Io(io_error),
            Err(error) => error,
        };
        let mut io_context: Option<(std::io::ErrorKind, String)> = None;
        for cause in error.chain() {
            if let Some(io_error) = cause.downcast_ref::<std::io::Error>() {
                // Context-wrapped I/O error: classify by the root cause but
                // keep the I/O error's own display, not the full chain text.
                // The payload keeps the original chain as its `source()` so
                // the underlying cause stays reachable.
                io_context = Some((io_error.kind(), io_error.to_string()));
                break;
            }
        }
        if let Some((kind, display)) = io_context {
            return Self::Io(std::io::Error::new(
                kind,
                IoWithSource {
                    display,
                    source: error,
                },
            ));
        }
        if boundary_session_errors {
            let text = error.to_string();
            // Run-path PRECONDITIONS are not session-storage failures: a second
            // concurrent prompt and a prompt submitted while a compaction owns
            // the session both reject before the run starts, and both are
            // reported as the catch-all `Other` so a caller matching
            // `SdkError::Session` keeps meaning "the transcript could not be
            // opened/read/written".
            if text.contains("already streaming") || text.contains("compaction is in progress") {
                return Self::Other(text);
            }
            return Self::Session(text);
        }
        Self::Other(error.to_string())
    }
}

/// I/O error payload that keeps the original error chain reachable via
/// [`std::error::Error::source`]: rebuilding with `std::io::Error::new(kind,
/// text)` keeps the display text but drops the source. The payload displays
/// exactly like the original I/O error and exposes the original `anyhow`
/// chain as its source.
#[derive(Debug)]
struct IoWithSource {
    /// The original I/O error's own display text.
    display: String,
    /// The original (context-wrapped) error, kept as the source chain.
    source: anyhow::Error,
}

impl std::fmt::Display for IoWithSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display)
    }
}

impl std::error::Error for IoWithSource {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // `anyhow::Error` implements `std::error::Error`, so the original
        // chain is reachable straight through the cast.
        Some(&*self.source as &(dyn std::error::Error + 'static))
    }
}

impl From<anyhow::Error> for SdkError {
    fn from(error: anyhow::Error) -> Self {
        Self::from_anyhow(error)
    }
}
