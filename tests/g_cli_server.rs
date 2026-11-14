// Merged test binary: one link unit for a whole subsystem (test-suite link
// time dominates `cargo test`; fewer binaries = faster compile). Each file
// keeps its own module namespace, so helpers cannot collide.
#[path = "suites/cli_config_resources.rs"]
mod cli_config_resources;
#[path = "suites/cli_entry_contract.rs"]
mod cli_entry_contract;
#[path = "suites/cli_metadata_trust.rs"]
mod cli_metadata_trust;
#[path = "suites/cli_package_install.rs"]
mod cli_package_install;
#[path = "suites/cli_parity_wave.rs"]
mod cli_parity_wave;
#[path = "suites/client_reconnect.rs"]
mod client_reconnect;
pub mod common;
#[path = "suites/g_server_persistence.rs"]
mod g_server_persistence;
#[path = "suites/mcp_config_shape.rs"]
mod mcp_config_shape;
#[path = "suites/powershell_tool.rs"]
mod powershell_tool;
#[path = "suites/print_retry_json.rs"]
mod print_retry_json;
#[path = "suites/rpc.rs"]
mod rpc;
#[path = "suites/rpc_commands.rs"]
mod rpc_commands;
#[path = "suites/rpc_extension_ui.rs"]
mod rpc_extension_ui;
#[path = "suites/rpc_tool_selection.rs"]
mod rpc_tool_selection;
#[path = "suites/server_prompt_abort.rs"]
mod server_prompt_abort;
#[path = "suites/server_tool_selection.rs"]
mod server_tool_selection;
#[path = "suites/server_unix.rs"]
mod server_unix;
#[path = "suites/session_pick_units.rs"]
mod session_pick_units;
#[path = "suites/session_resolve_units.rs"]
mod session_resolve_units;
#[path = "suites/settings_recovery.rs"]
mod settings_recovery;
#[path = "suites/timing_gates.rs"]
mod timing_gates;
