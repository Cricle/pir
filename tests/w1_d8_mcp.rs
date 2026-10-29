//! DRIFT W1 D8 (upstream `c5f5b3282` + `da0eadf04`):
//!
//! - RFC 9207: an authorization response's `iss` is rejected when no
//!   authorization server metadata was discovered (there is no issuer to
//!   compare with), instead of the check being skipped;
//! - a tool call abandoned while a shared OAuth refresh is in flight is not
//!   dispatched once the refresh returns: the manager re-checks the caller's
//!   signal before every dispatch attempt.

use std::path::Path;
use std::sync::Arc;

use serde_json::json;

use pir::ai::types::AbortSignal;
use pir::config::mcp::{parse_mcp_config, McpServerConfig};
use pir::mcp::error::McpError;
use pir::mcp::manager::McpManager;
use pir::mcp::oauth::{authorization_issuer_mismatch, AuthorizationServerMetadata};
use pir::mcp::registry::{McpServerEntry, McpServerSource};

fn metadata(issuer: &str, iss_supported: Option<bool>) -> AuthorizationServerMetadata {
    AuthorizationServerMetadata {
        issuer: issuer.to_string(),
        authorization_response_iss_parameter_supported: iss_supported,
        ..AuthorizationServerMetadata::default()
    }
}

/// Upstream `oauth.test.ts` (`exchanges a code only when its iss parameter
/// names the authorization server`): a matching `iss` and an omitted one on a
/// server that does not promise the parameter are fine; an `iss` that cannot
/// be verified — no metadata was discovered — is rejected; a missing `iss` on
/// a server that promised it is rejected too.
#[test]
fn iss_is_checked_against_the_discovered_metadata() {
    let server = metadata("https://auth.example", Some(true));

    // Matching issuer: exchanged.
    assert_eq!(
        authorization_issuer_mismatch(Some(&server), Some("https://auth.example")),
        None
    );
    // Servers that do not promise the parameter may omit it.
    assert_eq!(
        authorization_issuer_mismatch(Some(&metadata("https://auth.example", Some(false))), None),
        None
    );
    // A promised-but-missing `iss` is a mismatch.
    let mismatch = authorization_issuer_mismatch(Some(&server), None).expect("mismatch");
    assert_eq!(
        mismatch,
        "OAuth issuer mismatch: expected \"https://auth.example\", received none"
    );
    // A different issuer is rejected.
    assert!(
        authorization_issuer_mismatch(Some(&server), Some("https://attacker.example")).is_some()
    );

    // #10598's second half: without metadata there is no issuer to compare
    // with, so any `iss` is rejected before the code exchange (upstream
    // `OAuthIssuerMismatchError` with an undefined `expected`).
    let unverifiable =
        authorization_issuer_mismatch(None, Some("https://attacker.example")).expect("mismatch");
    assert_eq!(
        unverifiable,
        "OAuth issuer mismatch: expected none, received \"https://attacker.example\""
    );
    // Without metadata and without an `iss` there is nothing to check.
    assert_eq!(authorization_issuer_mismatch(None, None), None);
}

#[test]
fn aborted_error_carries_the_upstream_shape() {
    assert_eq!(McpError::Aborted.to_string(), "MCP request was aborted");
}

/// One registered HTTP server pointing at a dead port.
fn dead_http_server(cwd: &Path) -> Arc<McpManager> {
    let (servers, errors) =
        parse_mcp_config(r#"{ "mcpServers": { "dead": { "url": "http://127.0.0.1:1/mcp" } } }"#);
    assert!(errors.is_empty(), "{errors:?}");
    let config: McpServerConfig = servers.into_values().next().expect("server config");
    let manager = McpManager::without_log(cwd);
    manager.registry().register(McpServerEntry {
        name: "dead".to_string(),
        config,
        source: McpServerSource::GlobalConfig(cwd.join("mcp.json")),
    });
    manager
}

/// #10598: a call whose caller gave up is not dispatched — the manager
/// answers `Aborted` without touching the (dead) server, while the same call
/// with a live signal goes through and reports the transport failure.
#[tokio::test]
async fn an_abandoned_call_is_not_dispatched() {
    let cwd = std::env::temp_dir();
    let manager = dead_http_server(&cwd);

    let aborted = AbortSignal::new();
    aborted.abort();
    let error = manager
        .call_tool("dead", "search", &json!({}), Some(&aborted))
        .await
        .expect_err("aborted");
    assert!(
        matches!(error, McpError::Aborted),
        "the aborted call never reached the server: {error:?}"
    );

    // A live signal dispatches (and fails against the dead endpoint with a
    // transport error, never `Aborted`).
    let live = AbortSignal::new();
    let error = manager
        .call_tool("dead", "search", &json!({}), Some(&live))
        .await
        .expect_err("dead endpoint");
    assert!(
        !matches!(error, McpError::Aborted),
        "a live call is dispatched: {error:?}"
    );
}
