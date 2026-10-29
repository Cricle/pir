//! Tests for crate-level exports and library API surface.

use pir::Result;

#[test]
fn result_type_alias_is_anyhow() {
    fn ok() -> Result<i32> {
        Ok(42)
    }
    assert_eq!(ok().unwrap(), 42);
}

#[test]
fn result_type_alias_error() {
    fn err() -> Result<i32> {
        Err(anyhow::anyhow!("test error"))
    }
    assert!(err().is_err());
}

#[test]
fn result_type_can_use_anyhow_operators() {
    let r: Result<i32> = Ok(1);
    let mapped = r.map(|v| v + 1);
    assert_eq!(mapped.unwrap(), 2);
}

#[test]
fn lib_exports_agent_module() {
    let _ = pir::agent::tool_batch::ToolExecutionMode::default();
}

#[test]
fn lib_exports_ai_module() {
    let _ = pir::ai::types::StopReason::Stop;
}

#[test]
fn lib_exports_protocol_module() {
    assert_eq!(pir::protocol::PROTOCOL_VERSION, 8);
}

#[test]
fn lib_exports_config_module() {
    let config = pir::config::Config::default();
    assert_eq!(config.max_parallel_tools, 8);
}

#[test]
fn lib_exports_client_module() {
    let _ = pir::client::ClientError::Disconnected;
}

#[test]
fn lib_exports_server_module() {
    let _ = pir::server::ServerState::default();
}

#[test]
fn lib_exports_tui_module() {
    let _ = pir::tui::Container::new();
}

#[test]
fn lib_exports_coding_agent_module() {
    let _ = pir::coding_agent::register_core_tools;
}
