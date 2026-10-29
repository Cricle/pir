//! W16-H: the in-session sign-in guidance names the user's own commands.
//!
//! Upstream `signInRequiredMessage` (`extensions/mcp/runtime.ts:77-80`) reads
//! the remedy off the server entry's config:
//!
//! ```text
//! MCP server "<name>" requires sign-in. Run <remedy> to sign in.
//!   remedy = auth.provider ? `/login <provider>` : "/mcp"
//! ```
//!
//! Before this wave both pir sites (`mcp::oauth::sign_in_required_message` and
//! `McpError::LoginRequired`'s `Display`) said ``Run `pir mcp login <name>` ``,
//! which is the CLI form and does not exist for the model or an in-session
//! user. The CLI keeps its own `pir mcp login <name>` hint (`pir mcp add`),
//! asserted at the bottom of this suite.
//!
//! NOTE: registered by the foreman (`#[path = "suites/mcp_signin_text_w16.rs"]`
//! in a `g_*` merged binary). Self-contained: it needs no `mod common;`.

#![cfg(feature = "mcp")]

use serde_json::json;

use pir::config::mcp::McpServerConfig;
use pir::mcp::error::McpError;
use pir::mcp::oauth::{sign_in_required_message, sign_in_required_message_for};

fn config(value: serde_json::Value) -> McpServerConfig {
    serde_json::from_value(value).expect("valid server config")
}

/// An HTTP server without `auth`: the OAuth case, whose remedy is `/mcp`.
#[test]
fn server_without_auth_provider_points_at_the_mcp_command() {
    let http = config(json!({ "url": "https://mcp.example.com/mcp" }));
    assert_eq!(
        sign_in_required_message_for(&http, "docs"),
        "MCP server \"docs\" requires sign-in. Run /mcp to sign in."
    );

    // Upstream reads the provider only from a `url` entry; a stdio server has
    // none, so it takes the same `/mcp` arm.
    let stdio = config(json!({ "command": "npx", "args": ["-y", "mcp"] }));
    assert_eq!(
        sign_in_required_message_for(&stdio, "docs"),
        "MCP server \"docs\" requires sign-in. Run /mcp to sign in."
    );

    // The name-only form (used by call sites that already know the server is
    // OAuth-only, where `auth` is by construction absent) agrees.
    assert_eq!(
        sign_in_required_message("docs"),
        "MCP server \"docs\" requires sign-in. Run /mcp to sign in."
    );
}

/// `auth: { provider }`: the token comes from a pi provider, so the remedy is
/// that provider's `/login`.
#[test]
fn server_with_auth_provider_points_at_the_provider_login() {
    let http = config(json!({
        "url": "https://mcp.example.com/mcp",
        "auth": { "provider": "github-copilot" }
    }));
    assert_eq!(
        sign_in_required_message_for(&http, "docs"),
        "MCP server \"docs\" requires sign-in. Run /login github-copilot to sign in."
    );
}

/// The typed error carries the same in-session wording as the connection
/// state; the variant and its code are unchanged (only the text moved).
#[test]
fn login_required_error_carries_the_in_session_wording() {
    let message = McpError::LoginRequired {
        server: "docs".to_owned(),
    }
    .to_string();
    assert_eq!(
        message,
        "MCP server \"docs\" requires sign-in. Run /mcp to sign in."
    );
    assert!(
        !message.contains("pir mcp login"),
        "the agent-facing error must not point at the CLI: {message}"
    );
}

/// The CLI's own hint still names the CLI form: `pir mcp add` prints it for
/// an Authorization-less HTTP server (upstream `cli.ts:391`).
#[test]
fn cli_hint_still_names_the_cli_login_command() {
    let Some(binary) = option_env!("CARGO_BIN_EXE_pir") else {
        eprintln!("skipping: CARGO_BIN_EXE_pir is unavailable");
        return;
    };
    let agent_dir =
        std::env::temp_dir().join(format!("pir-w16h-mcp-signin-{}", std::process::id()));
    std::fs::create_dir_all(&agent_dir).expect("create scratch agent dir");
    let output = std::process::Command::new(binary)
        .env("PIR_AGENT_DIR", &agent_dir)
        .args([
            "mcp",
            "add",
            "w16h-weather",
            "--url",
            "https://mcp.example.com/mcp",
        ])
        .output()
        .expect("pir binary should start");
    let _ = std::fs::remove_dir_all(&agent_dir);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "pir mcp add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("If it requires sign-in: pir mcp login w16h-weather"),
        "the CLI keeps naming the CLI form: {stdout}"
    );
}
