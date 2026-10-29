// Merged test binary: one link unit for a whole subsystem (test-suite link
// time dominates `cargo test`; fewer binaries = faster compile). Each file
// keeps its own module namespace, so helpers cannot collide.
// `tests/common/mod.rs` is included once per test binary, so a static there is
// shared by every suite module of THIS binary — which is exactly what a
// process-wide env lock needs. The copy is duplicated per binary, and that is
// correct: separate test binaries are separate processes with separate
// environments, so their locks can never race each other.
mod common;

#[path = "suites/agent_core_w11.rs"]
mod agent_core_w11;
#[path = "suites/agent_core_w8.rs"]
mod agent_core_w8;
#[path = "suites/agent_dependencies.rs"]
mod agent_dependencies;
#[path = "suites/agent_dir_identity_w16.rs"]
mod agent_dir_identity_w16;
#[path = "suites/agent_loop.rs"]
mod agent_loop;
#[path = "suites/agent_loop_parity.rs"]
mod agent_loop_parity;
#[path = "suites/agent_session.rs"]
mod agent_session;
#[path = "suites/agent_session_units.rs"]
mod agent_session_units;
#[path = "suites/agent_tool_execution.rs"]
mod agent_tool_execution;
#[path = "suites/agent_tool_types.rs"]
mod agent_tool_types;
#[path = "suites/agent_tool_workflow.rs"]
mod agent_tool_workflow;
#[path = "suites/agent_tools.rs"]
mod agent_tools;
#[path = "suites/background_tool_calls.rs"]
mod background_tool_calls;
#[path = "suites/branch_summarization.rs"]
mod branch_summarization;
#[path = "suites/cache_warmer.rs"]
mod cache_warmer;
#[path = "suites/compaction.rs"]
mod compaction;
#[path = "suites/compaction_chunking.rs"]
mod compaction_chunking;
#[path = "suites/context_usage_projection_estimate.rs"]
mod context_usage_projection_estimate;
#[path = "suites/env_identity_w18.rs"]
mod env_identity_w18;
#[path = "suites/hooks_payload_w20.rs"]
mod hooks_payload_w20;
#[path = "suites/hooks_session_w21.rs"]
mod hooks_session_w21;
#[path = "suites/mcp_agent_dir_w16.rs"]
mod mcp_agent_dir_w16;
#[path = "suites/mcp_env_w11.rs"]
mod mcp_env_w11;
#[path = "suites/mcp_plugin_root_w17.rs"]
mod mcp_plugin_root_w17;
#[path = "suites/mcp_prune_w18.rs"]
mod mcp_prune_w18;
#[path = "suites/mcp_residuals_w13.rs"]
mod mcp_residuals_w13;
#[path = "suites/mcp_signin_text_w16.rs"]
mod mcp_signin_text_w16;
#[path = "suites/mcp_slash_command_w13.rs"]
mod mcp_slash_command_w13;
#[path = "suites/mcp_status_w13.rs"]
mod mcp_status_w13;
#[path = "suites/mcp_toggle_refresh_w18.rs"]
mod mcp_toggle_refresh_w18;
#[path = "suites/model_switch_auth_w17.rs"]
mod model_switch_auth_w17;
#[path = "suites/native_host_w19.rs"]
mod native_host_w19;
#[path = "suites/parallel_concurrency.rs"]
mod parallel_concurrency;
#[path = "suites/plugin_events_w16.rs"]
mod plugin_events_w16;
#[path = "suites/plugin_ledger_w14.rs"]
mod plugin_ledger_w14;
#[path = "suites/plugin_services_w10.rs"]
mod plugin_services_w10;
#[path = "suites/plugins_registry_gate.rs"]
mod plugins_registry_gate;
#[path = "suites/plugins_unified.rs"]
mod plugins_unified;
#[path = "suites/plugins_units.rs"]
mod plugins_units;
#[path = "suites/plugins_w11.rs"]
mod plugins_w11;
#[path = "suites/print_callability_w13.rs"]
mod print_callability_w13;
#[path = "suites/print_mode.rs"]
mod print_mode;
#[path = "suites/reload_native_w14.rs"]
mod reload_native_w14;
#[path = "suites/run_diagnostics.rs"]
mod run_diagnostics;
#[path = "suites/sdk.rs"]
mod sdk;
#[path = "suites/sdk_agent_isolation_w16.rs"]
mod sdk_agent_isolation_w16;
#[path = "suites/sdk_defaults_w13.rs"]
mod sdk_defaults_w13;
#[path = "suites/sdk_gaps_w11.rs"]
mod sdk_gaps_w11;
#[path = "suites/sdk_plugins_w8.rs"]
mod sdk_plugins_w8;
#[path = "suites/sdk_proxy_w13.rs"]
mod sdk_proxy_w13;
#[path = "suites/sdk_units.rs"]
mod sdk_units;
#[path = "suites/session_commands.rs"]
mod session_commands;
#[path = "suites/session_compaction_w11.rs"]
mod session_compaction_w11;
#[path = "suites/session_events_parity.rs"]
mod session_events_parity;
#[path = "suites/session_file_contract_w14.rs"]
mod session_file_contract_w14;
#[path = "suites/session_manager.rs"]
mod session_manager;
#[path = "suites/session_rewrite_w13.rs"]
mod session_rewrite_w13;
#[path = "suites/todo_builtin.rs"]
mod todo_builtin;
