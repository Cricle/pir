//! The `/mcp` list form: an undiscovered server must never read as an empty
//! server, and a dropped connection keeps its last known tool count.
//!
//! Moved out of `src/plugins/builtin/mcp.rs` — unit tests do not live in `src/`.

use std::path::PathBuf;

use pir_core::config::mcp::McpExposure;
use pir_core::mcp::manager::{ConnectionState, McpServerStatus, McpStatusSnapshot};
use pir_core::mcp::registry::McpServerSource;
use pir_core::plugins::builtin::mcp::servers_report;

fn server(state: ConnectionState, tools: Vec<String>) -> McpServerStatus {
    McpServerStatus {
        name: "docs".to_owned(),
        config: serde_json::from_value(serde_json::json!({ "command": "true" }))
            .expect("valid server config"),
        builtin_tools: false,
        exposure: McpExposure::Direct,
        enabled: true,
        state,
        tools,
        transport: "stdio".to_owned(),
        source: McpServerSource::GlobalConfig(PathBuf::from("mcp.json")),
    }
}

fn report(state: ConnectionState, tools: Vec<String>) -> String {
    servers_report(&McpStatusSnapshot {
        servers: vec![server(state, tools)],
        config_errors: Vec::new(),
        overridden: Vec::new(),
        agent_dir: PathBuf::from("."),
    })
}

/// A server that never published a discovery must not claim `0 tools`:
/// the state word on the same row already distinguishes configured,
/// connecting and failed.
#[test]
fn unconnected_zero_tools_is_not_reported_as_zero() {
    let states = [
        ConnectionState::Idle,
        ConnectionState::Connecting,
        ConnectionState::Failed {
            error: "boom".to_owned(),
        },
        ConnectionState::Disconnected { error: None },
        ConnectionState::Closed,
    ];
    for state in states {
        let text = report(state.clone(), Vec::new());
        assert!(
            !text.contains("0 tools"),
            "{state:?} must not read as an empty server: {text}"
        );
        assert!(
            text.contains("tools not discovered yet"),
            "{state:?} names the missing discovery: {text}"
        );
    }
}

/// A connected server's count is authoritative: zero means it really
/// exposes no tools.
#[test]
fn connected_zero_tools_stays_zero() {
    let text = report(ConnectionState::Connected, Vec::new());
    assert!(text.contains("0 tools"), "{text}");
    assert!(!text.contains("not discovered"), "{text}");
}

/// A dropped connection keeps the last discovered count instead of
/// claiming nothing was ever discovered.
#[test]
fn unconnected_last_known_count_is_kept() {
    let text = report(
        ConnectionState::Disconnected { error: None },
        vec!["echo".to_owned()],
    );
    assert!(text.contains("1 tool"), "{text}");
    assert!(!text.contains("not discovered"), "{text}");
}
