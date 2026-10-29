//! Typed MCP errors (upstream `packages/mcp/src/protocol/jsonrpc.ts` error
//! classes: `McpError`, `McpConnectionClosedError`, `McpTimeoutError`,
//! `McpHttpError`, `McpSessionExpiredError`).

/// JSON-RPC / MCP protocol error codes (upstream `JSON_RPC_ERROR_CODES`).
pub const JSON_RPC_ERROR_CODES: JsonRpcErrorCodes = JsonRpcErrorCodes;

/// Namespace so call sites read `McpError::CODES.parse_error` like upstream's
/// `JSON_RPC_ERROR_CODES.parseError`.
#[derive(Debug, Clone, Copy)]
pub struct JsonRpcErrorCodes;

impl JsonRpcErrorCodes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// Everything that can go wrong in the MCP client, transports, and manager.
///
/// `Clone` is derived: the I/O variant holds its `std::io::Error` in an `Arc`
/// (`std::io::Error` itself is not `Clone`), so cloning shares the original
/// error — kind, display text, and `source()` chain — instead of rebuilding it
/// from kind + text.
#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    /// Transport-level I/O failure (spawn, pipe write, socket).
    #[error("MCP transport I/O error: {0}")]
    Io(#[source] std::sync::Arc<std::io::Error>),
    /// A request exceeded its deadline.
    #[error("MCP request timed out after {timeout_ms}ms")]
    Timeout {
        /// The deadline that elapsed.
        timeout_ms: u64,
    },
    /// The transport dropped before an answer arrived (stdio server exit,
    /// closed HTTP stream).
    #[error("MCP connection closed")]
    ConnectionClosed,
    /// A transport-level request failure with its cause (upstream surfaces
    /// the underlying error instead of collapsing every failure to
    /// "connection closed"). The message carries a classification prefix
    /// written by `http::transport_error_from_chain` (`connection reset`-class
    /// failures are the only transient ones).
    #[error("MCP transport failure: {0}")]
    Transport(String),
    /// The peer answered with a JSON-RPC error object.
    #[error("MCP error {code}: {message}")]
    Server {
        /// JSON-RPC error code.
        code: i64,
        /// Error message from the server.
        message: String,
        /// Optional `data` payload.
        data: Option<serde_json::Value>,
    },
    /// A malformed JSON-RPC message arrived.
    #[error("MCP protocol error: {0}")]
    Protocol(String),
    /// A message exceeded the bounded-IO cap.
    #[error(
        "MCP message exceeds the {limit}-byte frame cap ({})",
        crate::mcp::transport::frame_cap_label(*.limit)
    )]
    MessageTooLarge {
        /// The configured cap.
        limit: usize,
    },
    /// HTTP transport got a non-2xx status (upstream `McpHttpError`).
    #[error("MCP HTTP request failed with status {status}{body}")]
    Http {
        /// Response status code.
        status: u16,
        /// Trimmed body snippet carried for diagnostics.
        body: String,
    },
    /// The HTTP session id the server assigned is no longer known (404 after
    /// a session was captured; upstream `McpSessionExpiredError`). Kept for
    /// upstream parity and the manager's retry arm; rmcp transparently
    /// re-handshakes expired sessions (`session_recovery_timeout`), so pir
    /// never constructs it today.
    #[error("MCP session expired")]
    SessionExpired,
    /// Invalid server configuration (naming, missing fields).
    #[error("MCP configuration error: {0}")]
    Config(String),
    /// The caller cancelled the request before it was dispatched (upstream
    /// `McpAbortError`, #10598): a call whose caller gave up while waiting on
    /// a shared OAuth refresh must not reach the server.
    #[error("MCP request was aborted")]
    Aborted,
    /// The named server is not in the registry.
    #[error("MCP server \"{0}\" is not configured")]
    UnknownServer(String),
    /// The named server exists but its entry is disabled.
    #[error("MCP server \"{0}\" is disabled")]
    ServerDisabled(String),
    /// A connection is usable only after the user signs in: the server
    /// answered an OAuth challenge that no stored credential satisfies
    /// (upstream `McpOAuthAuthorizationRequiredError` and
    /// `signInRequiredMessage`). The in-session `/mcp` command completes it;
    /// the text is shaped by [`crate::mcp::oauth::sign_in_required_message`],
    /// which keeps one wording for this error and the connection state.
    #[error("{}", crate::mcp::oauth::sign_in_required_message(.server))]
    LoginRequired {
        /// The server that needs the sign-in.
        server: String,
    },
    /// A connection whose config delegates its token to a pi provider
    /// (`auth: { provider }`) was rejected with a 401 and re-reading that
    /// provider's token did not satisfy the server: the user must sign in to
    /// the provider (upstream `needsSignIn` -> `signInRequiredMessage`'s
    /// `/login <provider>` arm, runtime.ts:77-80,318-323).
    #[error(
        "{}",
        crate::mcp::oauth::sign_in_required_message_provider(.server, .provider)
    )]
    ProviderLoginRequired {
        /// The server that needs the sign-in.
        server: String,
        /// The pi provider whose `/login` satisfies it.
        provider: String,
    },
}

impl From<std::io::Error> for McpError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(std::sync::Arc::new(error))
    }
}
