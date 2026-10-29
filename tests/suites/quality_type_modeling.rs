//! Quality-wave type-modeling tests (Q4): strum enum string round-trips,
//! derive_more `Display`, `McpError` clone fidelity, and `mcp.json`
//! `deny_unknown_fields` hardening. Every test pins the exact wire strings so
//! the derive-based replacements stay byte-identical to the hand-written
//! impls they replaced.

use std::error::Error as _;
use std::str::FromStr;

use pir::agent::tool_dependencies::ResourceKey;
use pir::ai::types::{ThinkingLevel, ThinkingTokenBudgetField};
use pir::config::mcp::parse_mcp_config;
use pir::mcp::McpError;

// ---------------------------------------------------------------------------
// ThinkingLevel: strum <-> serde wire names ("lowercase")
// ---------------------------------------------------------------------------

#[test]
fn thinking_level_string_round_trip() {
    const LEVELS: [(ThinkingLevel, &str); 7] = [
        (ThinkingLevel::Off, "off"),
        (ThinkingLevel::Minimal, "minimal"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::XHigh, "xhigh"),
        (ThinkingLevel::Max, "max"),
    ];
    for (level, key) in LEVELS {
        // The map-lookup key and the strum strings must stay identical.
        assert_eq!(level.wire_key(), key, "wire_key({key})");
        assert_eq!(level.to_string(), key, "Display({key})");
        assert_eq!(<ThinkingLevel as AsRef<str>>::as_ref(&level), key);
        assert_eq!(
            ThinkingLevel::from_str(key).unwrap(),
            level,
            "from_str({key})"
        );
        // serde wire names unchanged by the strum derives.
        assert_eq!(serde_json::to_string(&level).unwrap(), format!("\"{key}\""));
        assert_eq!(
            serde_json::from_str::<ThinkingLevel>(&format!("\"{key}\"")).unwrap(),
            level
        );
    }
    // Parsing stays exact: no case-insensitive slippage, no unknown levels.
    assert!(ThinkingLevel::from_str("OFF").is_err());
    assert!(ThinkingLevel::from_str("ultra").is_err());
    assert!(ThinkingLevel::from_str("").is_err());
}

#[test]
fn thinking_level_serde_rejects_unknown_names() {
    assert!(serde_json::from_str::<ThinkingLevel>("\"OFF\"").is_err());
    assert!(serde_json::from_str::<ThinkingLevel>("\"turbo\"").is_err());
}

// ---------------------------------------------------------------------------
// ThinkingTokenBudgetField: strum <-> serde wire names ("snake_case")
// ---------------------------------------------------------------------------

#[test]
fn thinking_token_budget_field_string_round_trip() {
    const FIELDS: [(ThinkingTokenBudgetField, &str); 3] = [
        (
            ThinkingTokenBudgetField::ThinkingTokenBudget,
            "thinking_token_budget",
        ),
        (ThinkingTokenBudgetField::ThinkingBudget, "thinking_budget"),
        (
            ThinkingTokenBudgetField::ThinkingBudgetTokens,
            "thinking_budget_tokens",
        ),
    ];
    for (field, name) in FIELDS {
        assert_eq!(field.wire_name(), name, "wire_name({name})");
        assert_eq!(field.to_string(), name, "Display({name})");
        assert_eq!(
            ThinkingTokenBudgetField::from_str(name).unwrap(),
            field,
            "from_str({name})"
        );
        assert_eq!(
            serde_json::to_string(&field).unwrap(),
            format!("\"{name}\"")
        );
        assert_eq!(
            serde_json::from_str::<ThinkingTokenBudgetField>(&format!("\"{name}\"")).unwrap(),
            field
        );
    }
    assert!(ThinkingTokenBudgetField::from_str("thinkingtokenbudget").is_err());
    assert!(ThinkingTokenBudgetField::from_str("thinkingTokenBudget").is_err());
}

// ---------------------------------------------------------------------------
// ResourceKey: derive_more Display forwards to the key string
// ---------------------------------------------------------------------------

#[test]
fn resource_key_display_is_the_key_string() {
    let key = ResourceKey::logical(
        std::path::Path::new("sub/./a/../b.txt"),
        std::path::Path::new("/work"),
    );
    assert_eq!(key.to_string(), key.as_str());
    assert_eq!(format!("[{key}]"), format!("[{}]", key.as_str()));
    // A cloned key displays identically.
    assert_eq!(key.clone().to_string(), key.to_string());
}

// ---------------------------------------------------------------------------
// McpError: derived Clone shares the I/O error instead of rebuilding it
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct RootCause;

impl std::fmt::Display for RootCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("root cause")
    }
}

impl std::error::Error for RootCause {}

/// An I/O error payload whose `source()` is a further cause, so a clone
/// fidelity test can check the whole chain survives.
#[derive(Debug)]
struct ChainedIo {
    root: RootCause,
}

impl std::fmt::Display for ChainedIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("chained io")
    }
}

impl std::error::Error for ChainedIo {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.root)
    }
}

#[test]
fn mcp_error_io_clone_is_faithful() {
    let error = McpError::from(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "reset by peer",
    ));
    let cloned = error.clone();
    assert_eq!(cloned.to_string(), error.to_string());
    assert_eq!(
        cloned.to_string(),
        "MCP transport I/O error: reset by peer",
        "display text unchanged by the Arc refactor"
    );
    assert!(matches!(
        &cloned,
        McpError::Io(io)
            if io.kind() == std::io::ErrorKind::ConnectionReset
                && io.to_string() == "reset by peer"
    ));
}

#[test]
fn mcp_error_io_clone_preserves_source_chain() {
    let error = McpError::from(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        ChainedIo { root: RootCause },
    ));
    let McpError::Io(original) = &error else {
        panic!("expected Io variant");
    };
    let cloned = error.clone();
    let McpError::Io(cloned_io) = &cloned else {
        panic!("expected Io variant");
    };
    // The clone shares the original error object, so kind/text/source are the
    // same values (the old hand-written clone rebuilt from kind + text and
    // lost the source chain).
    assert!(std::sync::Arc::ptr_eq(original, cloned_io), "shared error");
    assert_eq!(cloned_io.to_string(), original.to_string());
    assert_eq!(cloned_io.kind(), original.kind());

    // The source chain of the cloned McpError survives the clone: the shared
    // `Arc<io::Error>` (displaying "chained io") is the first source, and its
    // own `source()` reaches the root cause.
    let mut chain = 0;
    let mut cause: Option<&(dyn std::error::Error + 'static)> = cloned.source();
    while let Some(current) = cause {
        if chain == 0 {
            assert_eq!(current.to_string(), "chained io");
        }
        if chain == 1 {
            assert_eq!(current.to_string(), "root cause");
        }
        chain += 1;
        cause = current.source();
    }
    assert_eq!(chain, 2, "the full source chain survives the clone");
}

#[test]
fn mcp_error_non_io_variants_clone_faithfully() {
    let error = McpError::Server {
        code: -32601,
        message: "method not found".to_owned(),
        data: Some(serde_json::json!({"method": "tools/list"})),
    };
    let cloned = error.clone();
    assert_eq!(cloned.to_string(), error.to_string());
    assert!(matches!(
        cloned,
        McpError::Server { code, message, data }
            if code == -32601 && message == "method not found" && data.is_some()
    ));
}

// ---------------------------------------------------------------------------
// mcp.json: deny_unknown_fields rejects typo'd server fields
// ---------------------------------------------------------------------------

#[test]
fn mcp_config_rejects_unknown_shared_fields() {
    // `commnd` would previously be silently ignored (flatten), producing a
    // confusing "url must be non-empty" validation error instead.
    let (servers, errors) = parse_mcp_config(r#"{ "typo": { "commnd": "node" } }"#);
    assert!(servers.is_empty(), "typo'd entry must be rejected");
    assert!(
        errors.iter().any(|error| error.contains("commnd")),
        "the unknown key is named in the error: {errors:?}"
    );

    // Unknown keys on HTTP entries are caught the same way.
    let (servers, errors) =
        parse_mcp_config(r#"{ "web": { "url": "https://mcp.example.com/mcp", "bogus": true } }"#);
    assert!(servers.is_empty());
    assert!(errors.iter().any(|error| error.contains("bogus")));
}

#[test]
fn mcp_config_still_accepts_every_known_field() {
    let (servers, errors) = parse_mcp_config(
        r#"{
            "fs": {
                "command": "npx",
                "args": ["-y", "fs-mcp"],
                "env": {"KEY": "value"},
                "cwd": "/tmp",
                "type": "stdio",
                "exposure": "codemode",
                "description": "filesystem",
                "toolExposure": {"*": "direct"},
                "enabled": true,
                "timeout": 30
            },
            "web": {
                "type": "http",
                "url": "https://mcp.example.com/mcp",
                "headers": {"Authorization": "Bearer x"},
                "oauth": {
                    "clientId": "abc",
                    "clientSecret": "shh",
                    "callbackPort": 8080,
                    "callbackUrl": "http://localhost:8080",
                    "scope": "tools",
                    "clientName": "pir",
                    "clientRegistration": "dcr",
                    "authServerMetadataUrl": "https://mcp.example.com/.well-known"
                },
                "exposure": "deferred",
                "enabled": false,
                "timeout": 60
            }
        }"#,
    );
    assert!(
        errors.is_empty(),
        "no known field may be rejected: {errors:?}"
    );
    assert_eq!(servers.len(), 2);
    assert!(servers.contains_key("fs"));
    assert!(servers.contains_key("web"));
}

#[test]
fn mcp_config_rejects_unknown_oauth_fields() {
    // `clientID` (wrong case) must not be silently dropped: dynamic client
    // registration would then kick in with different credentials.
    let (servers, errors) = parse_mcp_config(
        r#"{ "web": { "url": "https://mcp.example.com/mcp", "oauth": { "clientID": "abc" } } }"#,
    );
    assert!(servers.is_empty());
    assert!(errors.iter().any(|error| error.contains("clientID")));
}
