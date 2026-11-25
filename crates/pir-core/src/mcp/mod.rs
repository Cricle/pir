//! The native Rust MCP (Model Context Protocol) client, built on the
//! official `rmcp` SDK (replacing the upstream TypeScript
//! `packages/mcp/src/**` client):
//!
//! - [`client`] - the rmcp-backed session: connection entry points (stdio
//!   command spec / streamable HTTP URL), per-operation deadlines, the
//!   negotiated server snapshot, rmcp -> pir protocol-type conversion, the
//!   stdio process-tree shutdown sweep, and the server-notification surface;
//! - [`stdio`] - stdio spawn on rmcp's child-process transport (CREATE_NO_WINDOW
//!   / process groups, piped stderr drain, process-tree kill helpers);
//! - [`http`] - streamable HTTP transport construction on rmcp's client plus
//!   pir's transport-error classification (transient `connection reset` vs
//!   connect/timeout causes) for the retry policy;
//! - [`registry`] - server registry, `mcp__<server>__<tool>` naming,
//!   `isMcpToolName`, and the tool-name matcher;
//! - [`manager`] - lazy connection management shared by the `mcp` builtin
//!   plugin and the `pir mcp` CLI;
//! - [`protocol`] - pir's upstream-shaped MCP result types (converted from
//!   rmcp's models by the client);
//! - [`error`] - the typed [`McpError`];
//! - [`log`] - the bounded `<agentDir>/mcp.log`;
//! - [`oauth`] - MCP OAuth (upstream `packages/mcp/src/oauth`): RFC 9728
//!   protected-resource + RFC 8414/OIDC metadata discovery, PKCE (S256) over
//!   a loopback redirect with a pasted-redirect-URL fallback, dynamic client
//!   registration, `state` + RFC 9207 `iss` validation, and the token store /
//!   refresh / bearer attach used by the streamable-HTTP transport;
//! - [`jsonrpc`] / [`sse`] - legacy hand-rolled units kept for the
//!   upstream-parity unit tests (`tests/mcp_units.rs`): the JSON-RPC message
//!   model (classification + builders) and the bounded SSE line parser; the
//!   wire protocol itself now belongs to rmcp.
//! - [`server`] - the built-in MCP **servers** pir itself hosts (feature
//!   `mcp-sd`): `pir mcp-sd [--root <dir>]` serves the `sd` search & replace
//!   tool (chmln/sd semantics) over stdio, so pir is both ends of MCP without
//!   a second executable.
//!
//! `pir mcp login/logout <server>` drive the OAuth flow; tokens persist in
//! `<agentDir>/mcp-auth.json` with a 30 s pre-expiry refresh skew plus a
//! single-flight refresh after a 401.
//!
//! Bounded I/O: the stdio reader enforces a hard per-frame cap
//! (`StdioSpawnOptions::max_message_bytes`, default 1 MiB) with incremental
//! growth; on overflow it stops reading, kills the server's process tree and
//! the client surfaces `McpError::MessageTooLarge`, so a single oversized
//! line cannot OOM the host. The streamable-HTTP transport is configured with
//! the same 1 MiB cap for one raw SSE event, and [`http::BoundedHttpClient`]
//! enforces it on plain-JSON response bodies (`Content-Length` and streamed
//! bytes). The client's recorded-notification buffer is bounded by count and
//! bytes. Structured/resource payload accumulation is capped in the `mcp`
//! builtin (page count + total bytes, with an explicit truncation marker).
//!
//! The `rmcp` dependency is pinned in `Cargo.toml` with the features needed
//! for a client: `client`, `transport-child-process`, and
//! `transport-streamable-http-client-reqwest` (+ `reqwest-native-tls`).

pub mod client;
pub mod error;
pub mod http;
pub mod jsonrpc;
pub mod log;
pub mod manager;
pub mod oauth;
pub mod protocol;
pub mod registry;
// The built-in `sd` MCP server (`pir mcp-sd`), hosted by this same binary.
#[cfg(feature = "mcp-sd")]
pub mod server;
pub mod sse;
pub mod stdio;
pub mod transport;

pub use client::{McpClient, ServerEvent};
pub use error::McpError;
pub use manager::{
    ConnectionState, DiscoveredTool, McpManager, McpServerStatus, McpStatusSnapshot,
    ServerReadiness,
};
pub use registry::{
    create_mcp_tool_name, is_mcp_tool_name, mcp_namespace, tool_name_matcher, McpServerEntry,
    McpServerRegistry,
};
