//! Bounded-IO caps shared across the MCP layer.
//!
//! The stdio transport enforces [`DEFAULT_MAX_MESSAGE_BYTES`] on every
//! JSON-RPC frame it reads from a server: the reader stops at the cap (plus
//! the one delimiter byte it needs to accept a frame of exactly the cap),
//! records the overflow, kills the server's process tree, and surfaces
//! [`super::error::McpError::MessageTooLarge`] to the caller (see
//! [`super::stdio`]).
//!
//! The streamable-HTTP transport is rmcp's. Its SSE path is bounded by
//! rmcp's configured `max_sse_event_size`, which pir sets to the same 1 MiB
//! [`DEFAULT_MAX_MESSAGE_BYTES`] frame cap used for stdio (rmcp applies it to
//! the raw event bytes before SSE parsing, see
//! [`HTTP_MAX_SSE_EVENT_BYTES_DEFAULT`]). rmcp 3.5's plain-`application/json`
//! response path buffers the whole body (`response.bytes()`) without a cap;
//! [`super::http::BoundedHttpClient`] replaces that backend and enforces
//! [`DEFAULT_MAX_MESSAGE_BYTES`] on both the declared `Content-Length` and the
//! streamed body, reporting overflow as [`super::error::McpError::MessageTooLarge`].
//!
//! The constants here also bound pir-owned buffers that are not protocol
//! frames: the stdio stderr tail.

/// Default cap on one JSON-RPC message (1 MiB, matching the bounded-IO rule).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Default cap on the retained stdio stderr tail.
pub const DEFAULT_MAX_STDERR_BYTES: usize = 64 * 1024;

/// rmcp's own default SSE event cap (16 MiB), kept here for documentation:
/// pir overrides it with [`DEFAULT_MAX_MESSAGE_BYTES`] in
/// [`super::http::streamable_http_config_with_auth`].
pub const HTTP_MAX_SSE_EVENT_BYTES_DEFAULT: usize = 16 * 1024 * 1024;

/// Human-readable label for a byte cap (`1048576` -> `1 MiB`), used by the
/// overflow error so it names both the exact byte count and the familiar
/// unit.
pub fn frame_cap_label(limit: usize) -> String {
    const MIB: usize = 1024 * 1024;
    const KIB: usize = 1024;
    if limit >= MIB && limit.is_multiple_of(MIB) {
        format!("{} MiB", limit / MIB)
    } else if limit >= KIB && limit.is_multiple_of(KIB) {
        format!("{} KiB", limit / KIB)
    } else {
        format!("{limit} bytes")
    }
}
