# Test Harness Map

> Generated 2026-11-13; rows re-derived and re-verified 2026-11-14 against the
> current tree (the on-disk `tests/zz_merged_*` harnesses and the `[[test]]`
> block) — every row still matches. Every row below was derived from, and is
> verified against, the real `#[path = "..."]` lines in the generated
> harnesses and the explicit `[[test]]` targets in `Cargo.toml`.

## TL;DR

- `tests/*.rs` and `tests/suites/*.rs` are **not** built as one cargo target each.
  `autotests = false` disables cargo auto-discovery; instead the 8 generated
  `tests/zz_merged_1.rs` .. `tests/zz_merged_8.rs` harnesses pull the files in
  with `#[path = "....rs"] mod ...;` (`scripts/gen_merged_tests.ps1`).
- A handful of `tests/*.rs` files are **standalone `[[test]]` targets** declared
  explicitly at the end of `Cargo.toml` (e.g. `g_durable`, which itself folds the
  `tests/suites/durable_*.rs` files into one binary).
- Run any single suite/module with a nextest filter, e.g.
  `cargo nextest run -p pir-sdk --test zz_merged_3 -E 'test(<module>::)'`.
  `scripts/which_harness.sh <suite|module|tests/path.rs>` prints the right
  harness and a ready-to-paste command.

## ⚠️ Naming N harnesses costs N test-binary links

Each `[[test]]` target (and each generated `zz_merged_N.rs`) is its own crate:
it is compiled and **statically linked against the whole `pir-sdk`** (RustPython,
QuickJS, SWC, ...). Naming `N` harnesses therefore means `N` link steps, and link
time (not test execution) dominates the build. This is exactly why the 337
original root test files are folded into the 8 merged harnesses (plus the 5
standalone targets). Before adding a new
`[[test]]` entry / new harness, ask whether the suites can join an existing one
(`g_agent`/`g_ai`/`g_tui`/`g_tools`/`g_durable`/... already aggregate the
`tests/suites/*.rs` files); every extra name multiplies the link phase.

> A merged binary shares one process between its test functions, so env vars and
> process-global statics can interact. Run the merged harnesses under
> **nextest** (process-per-test), not bare `cargo test`.

## Targets (`Cargo.toml` `[[test]]`)

| target | path | kind | suites/root files compiled by it |
|---|---|---|---|
| `zz_merged_1` | `tests/zz_merged_1.rs` | merged harness | 67 |
| `zz_merged_2` | `tests/zz_merged_2.rs` | merged harness | 79 |
| `zz_merged_3` | `tests/zz_merged_3.rs` | merged harness | 80 |
| `zz_merged_4` | `tests/zz_merged_4.rs` | merged harness | 86 |
| `zz_merged_5` | `tests/zz_merged_5.rs` | merged harness | 81 |
| `zz_merged_6` | `tests/zz_merged_6.rs` | merged harness | 67 |
| `zz_merged_7` | `tests/zz_merged_7.rs` | merged harness | 52 |
| `zz_merged_8` | `tests/zz_merged_8.rs` | merged harness | 69 |
| `python_session_lifecycle` | `tests/python_session_lifecycle.rs` | standalone | 1 |
| `worker_threads_real` | `tests/worker_threads_real.rs` | standalone | 1 |
| `durable_memcopy_bench` | `tests/durable_memcopy_bench.rs` | standalone | 1 |
| `g_durable` | `tests/g_durable.rs` | standalone | 46 |
| `print_native_session` | `tests/print_native_session.rs` | standalone | 1 |

## Run ONE suite quickly

The general form (nextest filter expressions use substring match, so the
trailing `::` is all you need to scope to one module):

```bash
cargo nextest run -p pir-sdk --test <harness> -E 'test(<module>::)'
# e.g.
cargo nextest run -p pir-sdk --test zz_merged_3 -E 'test(g_extensions::)'
```

Concrete examples:

```bash
# a root test file merged directly into a zz_merged harness
cargo nextest run -p pir-sdk --test zz_merged_1 -E 'test(agent_bootstrap::)'

# a tests/suites/*.rs file folded into a g_* wrapper (here zz_merged_3)
cargo nextest run -p pir-sdk --test zz_merged_3 -E 'test(g_extensions::extensions_events::)'

# a tests/suites/*.rs file in the standalone g_durable target
cargo nextest run -p pir-sdk --test g_durable -E 'test(durable_assemble::)'

# the whole harness
cargo nextest run -p pir-sdk --test zz_merged_3
```

Use `scripts/which_harness.sh` to get exactly this for any name or path:

```bash
scripts/which_harness.sh durable_assemble
scripts/which_harness.sh tests/suites/durable_assemble.rs
scripts/which_harness.sh tests/agent_benchmark.rs
```

## Mapping — `tests/suites/*.rs` (294 files)

`wrapper` is the `tests/*.rs` file that includes the suite with
`#[path = "suites/....rs"]`; the wrapper is then included by the harness (or is a
standalone `[[test]]` target). The filter column is the exact nextest `test(...)`
predicate for that suite inside that harness.

| # | suites file | wrapper | harness target | nextest filter |
|---:|---|---|---|---|
| 1 | `tests/suites/agent_bootstrap.rs` | `tests/agent_bootstrap.rs` | `zz_merged_1` | `-E 'test(agent_bootstrap::agent_bootstrap::)'` |
| 2 | `tests/suites/cbor.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::cbor::)'` |
| 3 | `tests/suites/config_migrations.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::config_migrations::)'` |
| 4 | `tests/suites/config_schemas.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::config_schemas::)'` |
| 5 | `tests/suites/config_settings_units.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::config_settings_units::)'` |
| 6 | `tests/suites/config_units.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::config_units::)'` |
| 7 | `tests/suites/forbidden_files.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::forbidden_files::)'` |
| 8 | `tests/suites/framing.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::framing::)'` |
| 9 | `tests/suites/lib_exports.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::lib_exports::)'` |
| 10 | `tests/suites/orchestration_convergence.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::orchestration_convergence::)'` |
| 11 | `tests/suites/production_safety.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::production_safety::)'` |
| 12 | `tests/suites/protocol.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::protocol::)'` |
| 13 | `tests/suites/schema_generator_w11.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::schema_generator_w11::)'` |
| 14 | `tests/suites/source_hygiene.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::source_hygiene::)'` |
| 15 | `tests/suites/upstream_examples.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::upstream_examples::)'` |
| 16 | `tests/suites/codemode_builtin.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::codemode_builtin::)'` |
| 17 | `tests/suites/codemode_strict.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::codemode_strict::)'` |
| 18 | `tests/suites/codemode_worker.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::codemode_worker::)'` |
| 19 | `tests/suites/extensions_audit_fixes.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_audit_fixes::)'` |
| 20 | `tests/suites/extensions_events.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_events::)'` |
| 21 | `tests/suites/extensions_js_stream.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_js_stream::)'` |
| 22 | `tests/suites/extensions_limits.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_limits::)'` |
| 23 | `tests/suites/extensions_regressions.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_regressions::)'` |
| 24 | `tests/suites/extensions_stdio_routing.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_stdio_routing::)'` |
| 25 | `tests/suites/extensions_wiring.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_wiring::)'` |
| 26 | `tests/suites/js_extension_api.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::js_extension_api::)'` |
| 27 | `tests/suites/npm_resolve_units.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::npm_resolve_units::)'` |
| 28 | `tests/suites/print_request_tools_w14.rs` | `tests/g_print_tools_w14.rs` | `zz_merged_3` | `-E 'test(g_print_tools_w14::print_request_tools_w14::)'` |
| 29 | `tests/suites/subagent_strict.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::subagent_strict::)'` |
| 30 | `tests/suites/system_prompt_options_units.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::system_prompt_options_units::)'` |
| 31 | `tests/suites/user_bash_sink.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::user_bash_sink::)'` |
| 32 | `tests/suites/cli_config_resources.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::cli_config_resources::)'` |
| 33 | `tests/suites/cli_entry_contract.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::cli_entry_contract::)'` |
| 34 | `tests/suites/cli_metadata_trust.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::cli_metadata_trust::)'` |
| 35 | `tests/suites/cli_package_install.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::cli_package_install::)'` |
| 36 | `tests/suites/cli_parity_wave.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::cli_parity_wave::)'` |
| 37 | `tests/suites/client_reconnect.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::client_reconnect::)'` |
| 38 | `tests/suites/g_server_persistence.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::g_server_persistence::)'` |
| 39 | `tests/suites/mcp_config_shape.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::mcp_config_shape::)'` |
| 40 | `tests/suites/powershell_tool.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::powershell_tool::)'` |
| 41 | `tests/suites/print_retry_json.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::print_retry_json::)'` |
| 42 | `tests/suites/rpc.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::rpc::)'` |
| 43 | `tests/suites/rpc_commands.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::rpc_commands::)'` |
| 44 | `tests/suites/rpc_extension_ui.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::rpc_extension_ui::)'` |
| 45 | `tests/suites/rpc_tool_selection.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::rpc_tool_selection::)'` |
| 46 | `tests/suites/server_prompt_abort.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::server_prompt_abort::)'` |
| 47 | `tests/suites/server_tool_selection.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::server_tool_selection::)'` |
| 48 | `tests/suites/server_unix.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::server_unix::)'` |
| 49 | `tests/suites/session_pick_units.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::session_pick_units::)'` |
| 50 | `tests/suites/session_resolve_units.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::session_resolve_units::)'` |
| 51 | `tests/suites/settings_recovery.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::settings_recovery::)'` |
| 52 | `tests/suites/timing_gates.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::timing_gates::)'` |
| 53 | `tests/suites/tui_snapshot_refresh_w16.rs` | `tests/tui_snapshot_refresh_w16.rs` | `zz_merged_4` | `-E 'test(tui_snapshot_refresh_w16::tui_snapshot_refresh_w16::)'` |
| 54 | `tests/suites/builtin_disable_w13.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::builtin_disable_w13::)'` |
| 55 | `tests/suites/coding_agent_units.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::coding_agent_units::)'` |
| 56 | `tests/suites/context_mode_exposure_w13.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::context_mode_exposure_w13::)'` |
| 57 | `tests/suites/crash_log.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::crash_log::)'` |
| 58 | `tests/suites/export_html.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::export_html::)'` |
| 59 | `tests/suites/g11_context_shadow.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::g11_context_shadow::)'` |
| 60 | `tests/suites/g11_skills_diagnostics.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::g11_skills_diagnostics::)'` |
| 61 | `tests/suites/prompt_loadout_w22.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::prompt_loadout_w22::)'` |
| 62 | `tests/suites/python_vm_lock.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::python_vm_lock::)'` |
| 63 | `tests/suites/read_image_tool.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::read_image_tool::)'` |
| 64 | `tests/suites/shell_tools.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::shell_tools::)'` |
| 65 | `tests/suites/system_prompt.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::system_prompt::)'` |
| 66 | `tests/suites/tool_exposure_parity_w14.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_exposure_parity_w14::)'` |
| 67 | `tests/suites/tool_exposure_parity_w16.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_exposure_parity_w16::)'` |
| 68 | `tests/suites/tool_exposure_virtual_models.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_exposure_virtual_models::)'` |
| 69 | `tests/suites/tool_parity_h.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_parity_h::)'` |
| 70 | `tests/suites/tool_repair.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_repair::)'` |
| 71 | `tests/suites/tool_repair_grammar.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_repair_grammar::)'` |
| 72 | `tests/suites/tool_result_size_w17.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_result_size_w17::)'` |
| 73 | `tests/suites/tool_schema_validation.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_schema_validation::)'` |
| 74 | `tests/suites/tool_schema_w11.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_schema_w11::)'` |
| 75 | `tests/suites/tool_selection_parity_w13.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_selection_parity_w13::)'` |
| 76 | `tests/suites/tool_selection_units.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_selection_units::)'` |
| 77 | `tests/suites/tools_ab_round.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tools_ab_round::)'` |
| 78 | `tests/suites/tools_hardening.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tools_hardening::)'` |
| 79 | `tests/suites/tools_security_hardening.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tools_security_hardening::)'` |
| 80 | `tests/suites/tools_security_w9.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tools_security_w9::)'` |
| 81 | `tests/suites/tui_ansi_theme.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tui_ansi_theme::)'` |
| 82 | `tests/suites/write_persistence_nn.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::write_persistence_nn::)'` |
| 83 | `tests/suites/agent_core_w11.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_core_w11::)'` |
| 84 | `tests/suites/agent_core_w8.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_core_w8::)'` |
| 85 | `tests/suites/agent_dependencies.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_dependencies::)'` |
| 86 | `tests/suites/agent_dir_identity_w16.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_dir_identity_w16::)'` |
| 87 | `tests/suites/agent_loop.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_loop::)'` |
| 88 | `tests/suites/agent_loop_parity.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_loop_parity::)'` |
| 89 | `tests/suites/agent_loop_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_loop_units::)'` |
| 90 | `tests/suites/agent_session.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_session::)'` |
| 91 | `tests/suites/agent_session_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_session_units::)'` |
| 92 | `tests/suites/agent_tool_execution.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_tool_execution::)'` |
| 93 | `tests/suites/agent_tool_types.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_tool_types::)'` |
| 94 | `tests/suites/agent_tool_workflow.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_tool_workflow::)'` |
| 95 | `tests/suites/agent_tools.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_tools::)'` |
| 96 | `tests/suites/background_tool_calls.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::background_tool_calls::)'` |
| 97 | `tests/suites/branch_summarization.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::branch_summarization::)'` |
| 98 | `tests/suites/cache_warmer.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::cache_warmer::)'` |
| 99 | `tests/suites/compaction.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::compaction::)'` |
| 100 | `tests/suites/compaction_chunking.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::compaction_chunking::)'` |
| 101 | `tests/suites/context_usage_projection_estimate.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::context_usage_projection_estimate::)'` |
| 102 | `tests/suites/env_identity_w18.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::env_identity_w18::)'` |
| 103 | `tests/suites/hooks_payload_w20.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::hooks_payload_w20::)'` |
| 104 | `tests/suites/hooks_session_w21.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::hooks_session_w21::)'` |
| 105 | `tests/suites/mcp_agent_dir_w16.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_agent_dir_w16::)'` |
| 106 | `tests/suites/mcp_env_w11.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_env_w11::)'` |
| 107 | `tests/suites/mcp_plugin_root_w17.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_plugin_root_w17::)'` |
| 108 | `tests/suites/mcp_prune_w18.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_prune_w18::)'` |
| 109 | `tests/suites/mcp_residuals_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_residuals_w13::)'` |
| 110 | `tests/suites/mcp_signin_text_w16.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_signin_text_w16::)'` |
| 111 | `tests/suites/mcp_slash_command_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_slash_command_w13::)'` |
| 112 | `tests/suites/mcp_status_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_status_w13::)'` |
| 113 | `tests/suites/mcp_toggle_refresh_w18.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_toggle_refresh_w18::)'` |
| 114 | `tests/suites/memory_builtin.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::memory_builtin::)'` |
| 115 | `tests/suites/model_switch_auth_w17.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::model_switch_auth_w17::)'` |
| 116 | `tests/suites/native_host_w19.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::native_host_w19::)'` |
| 117 | `tests/suites/parallel_concurrency.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::parallel_concurrency::)'` |
| 118 | `tests/suites/phone_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::phone_units::)'` |
| 119 | `tests/suites/plugin_events_w16.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugin_events_w16::)'` |
| 120 | `tests/suites/plugin_ledger_w14.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugin_ledger_w14::)'` |
| 121 | `tests/suites/plugin_services_w10.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugin_services_w10::)'` |
| 122 | `tests/suites/plugins_registry_gate.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugins_registry_gate::)'` |
| 123 | `tests/suites/plugins_unified.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugins_unified::)'` |
| 124 | `tests/suites/plugins_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugins_units::)'` |
| 125 | `tests/suites/plugins_w11.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugins_w11::)'` |
| 126 | `tests/suites/print_callability_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::print_callability_w13::)'` |
| 127 | `tests/suites/print_mode.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::print_mode::)'` |
| 128 | `tests/suites/reload_native_w14.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::reload_native_w14::)'` |
| 129 | `tests/suites/resume_system_prompt_once.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::resume_system_prompt_once::)'` |
| 130 | `tests/suites/run_diagnostics.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::run_diagnostics::)'` |
| 131 | `tests/suites/sdk.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk::)'` |
| 132 | `tests/suites/sdk_agent_isolation_w16.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_agent_isolation_w16::)'` |
| 133 | `tests/suites/sdk_defaults_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_defaults_w13::)'` |
| 134 | `tests/suites/sdk_event_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_event_units::)'` |
| 135 | `tests/suites/sdk_gaps_w11.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_gaps_w11::)'` |
| 136 | `tests/suites/sdk_plugins_w8.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_plugins_w8::)'` |
| 137 | `tests/suites/sdk_proxy_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_proxy_w13::)'` |
| 138 | `tests/suites/sdk_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_units::)'` |
| 139 | `tests/suites/session_commands.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_commands::)'` |
| 140 | `tests/suites/session_compaction_w11.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_compaction_w11::)'` |
| 141 | `tests/suites/session_ctx_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_ctx_units::)'` |
| 142 | `tests/suites/session_events_parity.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_events_parity::)'` |
| 143 | `tests/suites/session_file_contract_w14.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_file_contract_w14::)'` |
| 144 | `tests/suites/session_manager.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_manager::)'` |
| 145 | `tests/suites/session_rewrite_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_rewrite_w13::)'` |
| 146 | `tests/suites/startup_thinking_clamp.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::startup_thinking_clamp::)'` |
| 147 | `tests/suites/todo_builtin.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::todo_builtin::)'` |
| 148 | `tests/suites/keybinding_actions.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::keybinding_actions::)'` |
| 149 | `tests/suites/std_utils_w11.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::std_utils_w11::)'` |
| 150 | `tests/suites/tui_alloc_trace_units.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_alloc_trace_units::)'` |
| 151 | `tests/suites/tui_cache_lane.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_cache_lane::)'` |
| 152 | `tests/suites/tui_clipboard_paste.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_clipboard_paste::)'` |
| 153 | `tests/suites/tui_core.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_core::)'` |
| 154 | `tests/suites/tui_custom_overlay_w13.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_custom_overlay_w13::)'` |
| 155 | `tests/suites/tui_editor.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_editor::)'` |
| 156 | `tests/suites/tui_editor_parity.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_editor_parity::)'` |
| 157 | `tests/suites/tui_exit_surfaces.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_exit_surfaces::)'` |
| 158 | `tests/suites/tui_extension_ui.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_extension_ui::)'` |
| 159 | `tests/suites/tui_first_run_setup.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_first_run_setup::)'` |
| 160 | `tests/suites/tui_followup_parity.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_followup_parity::)'` |
| 161 | `tests/suites/tui_gaps_w11.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_gaps_w11::)'` |
| 162 | `tests/suites/tui_grapheme_editor.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_grapheme_editor::)'` |
| 163 | `tests/suites/tui_image_preview_units.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_image_preview_units::)'` |
| 164 | `tests/suites/tui_input_wakeup.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_input_wakeup::)'` |
| 165 | `tests/suites/tui_keybindings_upstream.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_keybindings_upstream::)'` |
| 166 | `tests/suites/tui_live_settings_w14.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_live_settings_w14::)'` |
| 167 | `tests/suites/tui_markdown_autolinks_tables.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_markdown_autolinks_tables::)'` |
| 168 | `tests/suites/tui_markdown_incremental.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_markdown_incremental::)'` |
| 169 | `tests/suites/tui_markdown_parity.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_markdown_parity::)'` |
| 170 | `tests/suites/tui_messages_units.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_messages_units::)'` |
| 171 | `tests/suites/tui_output_writer_w10.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_output_writer_w10::)'` |
| 172 | `tests/suites/tui_overlay_float_w14.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_overlay_float_w14::)'` |
| 173 | `tests/suites/tui_parity_round2.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_parity_round2::)'` |
| 174 | `tests/suites/tui_parity_wave.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_parity_wave::)'` |
| 175 | `tests/suites/tui_program_status.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_program_status::)'` |
| 176 | `tests/suites/tui_r_altscreen.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_r_altscreen::)'` |
| 177 | `tests/suites/tui_read_header_w14.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_read_header_w14::)'` |
| 178 | `tests/suites/tui_refresh.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_refresh::)'` |
| 179 | `tests/suites/tui_regressions.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_regressions::)'` |
| 180 | `tests/suites/tui_render_hotpath_w10.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_render_hotpath_w10::)'` |
| 181 | `tests/suites/tui_render_residue.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_render_residue::)'` |
| 182 | `tests/suites/tui_renderer_w9.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_renderer_w9::)'` |
| 183 | `tests/suites/tui_session_cwd.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_session_cwd::)'` |
| 184 | `tests/suites/tui_session_manage.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_session_manage::)'` |
| 185 | `tests/suites/tui_sessions.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_sessions::)'` |
| 186 | `tests/suites/tui_settings_diagnostics.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_settings_diagnostics::)'` |
| 187 | `tests/suites/tui_slash_commands.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_slash_commands::)'` |
| 188 | `tests/suites/tui_stdin_decoding.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_stdin_decoding::)'` |
| 189 | `tests/suites/tui_surface_units.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_surface_units::)'` |
| 190 | `tests/suites/tui_syntax_highlight.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_syntax_highlight::)'` |
| 191 | `tests/suites/tui_system_theme.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_system_theme::)'` |
| 192 | `tests/suites/tui_theme_colors.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_theme_colors::)'` |
| 193 | `tests/suites/tui_theme_g9.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_theme_g9::)'` |
| 194 | `tests/suites/tui_tool_display.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_tool_display::)'` |
| 195 | `tests/suites/tui_tool_images_w14.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_tool_images_w14::)'` |
| 196 | `tests/suites/tui_units.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_units::)'` |
| 197 | `tests/suites/tui_widget_defer_w10.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_widget_defer_w10::)'` |
| 198 | `tests/suites/ai_abort_partial_w13.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_abort_partial_w13::)'` |
| 199 | `tests/suites/ai_abort_signal.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_abort_signal::)'` |
| 200 | `tests/suites/ai_adapter_fixes.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_adapter_fixes::)'` |
| 201 | `tests/suites/ai_adapter_parity.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_adapter_parity::)'` |
| 202 | `tests/suites/ai_adapters_parity.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_adapters_parity::)'` |
| 203 | `tests/suites/ai_adapters_w11.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_adapters_w11::)'` |
| 204 | `tests/suites/ai_auth.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_auth::)'` |
| 205 | `tests/suites/ai_bedrock_credentials.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_bedrock_credentials::)'` |
| 206 | `tests/suites/ai_catalog_full_w10.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_catalog_full_w10::)'` |
| 207 | `tests/suites/ai_catalog_w10.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_catalog_w10::)'` |
| 208 | `tests/suites/ai_codex_ws_w10.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_codex_ws_w10::)'` |
| 209 | `tests/suites/ai_codex_ws_w13.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_codex_ws_w13::)'` |
| 210 | `tests/suites/ai_copilot_headers.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_copilot_headers::)'` |
| 211 | `tests/suites/ai_error.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_error::)'` |
| 212 | `tests/suites/ai_event_stream.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_event_stream::)'` |
| 213 | `tests/suites/ai_event_stream_backpressure.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_event_stream_backpressure::)'` |
| 214 | `tests/suites/ai_event_stream_drop_w13.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_event_stream_drop_w13::)'` |
| 215 | `tests/suites/ai_event_stream_ext.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_event_stream_ext::)'` |
| 216 | `tests/suites/ai_google.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_google::)'` |
| 217 | `tests/suites/ai_http.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_http::)'` |
| 218 | `tests/suites/ai_malformed_args_w10.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_malformed_args_w10::)'` |
| 219 | `tests/suites/ai_pi_messages_cow.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_pi_messages_cow::)'` |
| 220 | `tests/suites/ai_producer_liveness_w13.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_producer_liveness_w13::)'` |
| 221 | `tests/suites/ai_provider.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_provider::)'` |
| 222 | `tests/suites/ai_provider_contracts.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_provider_contracts::)'` |
| 223 | `tests/suites/ai_remaining_adapter_contracts.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_remaining_adapter_contracts::)'` |
| 224 | `tests/suites/ai_responses_tool_changes.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_responses_tool_changes::)'` |
| 225 | `tests/suites/ai_retry_overflow_d2.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_retry_overflow_d2::)'` |
| 226 | `tests/suites/ai_sse.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_sse::)'` |
| 227 | `tests/suites/ai_sse_proptest.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_sse_proptest::)'` |
| 228 | `tests/suites/ai_telemetry_w11.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_telemetry_w11::)'` |
| 229 | `tests/suites/ai_transcript.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_transcript::)'` |
| 230 | `tests/suites/ai_transport_idle.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_transport_idle::)'` |
| 231 | `tests/suites/ai_types.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_types::)'` |
| 232 | `tests/suites/ai_util.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_util::)'` |
| 233 | `tests/suites/auth_cli.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::auth_cli::)'` |
| 234 | `tests/suites/auth_oauth.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::auth_oauth::)'` |
| 235 | `tests/suites/auth_oauth_unit.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::auth_oauth_unit::)'` |
| 236 | `tests/suites/builtins_cross.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::builtins_cross::)'` |
| 237 | `tests/suites/catalog_drift.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::catalog_drift::)'` |
| 238 | `tests/suites/catalog_units.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::catalog_units::)'` |
| 239 | `tests/suites/config_cache_lifetime_w16.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::config_cache_lifetime_w16::)'` |
| 240 | `tests/suites/g11_provider_attribution.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::g11_provider_attribution::)'` |
| 241 | `tests/suites/globset_w11.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::globset_w11::)'` |
| 242 | `tests/suites/model_catalog.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::model_catalog::)'` |
| 243 | `tests/suites/model_catalog_strict.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::model_catalog_strict::)'` |
| 244 | `tests/suites/model_resolution_units.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::model_resolution_units::)'` |
| 245 | `tests/suites/pi_config_trust.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::pi_config_trust::)'` |
| 246 | `tests/suites/provider_config_units.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::provider_config_units::)'` |
| 247 | `tests/suites/quality_type_modeling.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::quality_type_modeling::)'` |
| 248 | `tests/suites/schema_catalog_w11.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::schema_catalog_w11::)'` |
| 249 | `tests/suites/telemetry_core_w10.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::telemetry_core_w10::)'` |
| 250 | `tests/suites/durable_assemble.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_assemble::)'` |
| 251 | `tests/suites/durable_attach.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_attach::)'` |
| 252 | `tests/suites/durable_builtin_documents.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_builtin_documents::)'` |
| 253 | `tests/suites/durable_chord_diff.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_chord_diff::)'` |
| 254 | `tests/suites/durable_compaction.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_compaction::)'` |
| 255 | `tests/suites/durable_context.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_context::)'` |
| 256 | `tests/suites/durable_context_derive.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_context_derive::)'` |
| 257 | `tests/suites/durable_definitions.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_definitions::)'` |
| 258 | `tests/suites/durable_delta.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_delta::)'` |
| 259 | `tests/suites/durable_document_cache.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_document_cache::)'` |
| 260 | `tests/suites/durable_events.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_events::)'` |
| 261 | `tests/suites/durable_facade.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_facade::)'` |
| 262 | `tests/suites/durable_forks.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_forks::)'` |
| 263 | `tests/suites/durable_generation.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_generation::)'` |
| 264 | `tests/suites/durable_guard.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_guard::)'` |
| 265 | `tests/suites/durable_harness.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_harness::)'` |
| 266 | `tests/suites/durable_harness_agent.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_harness_agent::)'` |
| 267 | `tests/suites/durable_harness_documents.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_harness_documents::)'` |
| 268 | `tests/suites/durable_harness_settings.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_harness_settings::)'` |
| 269 | `tests/suites/durable_inbox.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_inbox::)'` |
| 270 | `tests/suites/durable_json.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_json::)'` |
| 271 | `tests/suites/durable_lifecycle_w8.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_lifecycle_w8::)'` |
| 272 | `tests/suites/durable_live.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_live::)'` |
| 273 | `tests/suites/durable_observation_units.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_observation_units::)'` |
| 274 | `tests/suites/durable_observer.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_observer::)'` |
| 275 | `tests/suites/durable_primitives.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_primitives::)'` |
| 276 | `tests/suites/durable_prompt.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_prompt::)'` |
| 277 | `tests/suites/durable_publication.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_publication::)'` |
| 278 | `tests/suites/durable_registry.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_registry::)'` |
| 279 | `tests/suites/durable_residuals_w11.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_residuals_w11::)'` |
| 280 | `tests/suites/durable_scheduler.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_scheduler::)'` |
| 281 | `tests/suites/durable_sqlite_seam.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_sqlite_seam::)'` |
| 282 | `tests/suites/durable_storage_conformance.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_storage_conformance::)'` |
| 283 | `tests/suites/durable_submissions.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_submissions::)'` |
| 284 | `tests/suites/durable_task_definition.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_task_definition::)'` |
| 285 | `tests/suites/durable_task_graph.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_task_graph::)'` |
| 286 | `tests/suites/durable_tool.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_tool::)'` |
| 287 | `tests/suites/durable_trace_w8.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_trace_w8::)'` |
| 288 | `tests/suites/durable_transaction.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_transaction::)'` |
| 289 | `tests/suites/durable_usage.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_usage::)'` |
| 290 | `tests/suites/durable_util.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_util::)'` |
| 291 | `tests/suites/durable_view.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_view::)'` |
| 292 | `tests/suites/pi_format_export.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(pi_format_export::)'` |
| 293 | `tests/suites/pi_format_import.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(pi_format_import::)'` |
| 294 | `tests/suites/pi_format_roundtrip.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(pi_format_roundtrip::)'` |

## Mapping — `tests/*.rs` root test files (337 compiled + 6 uncompiled)

Root files that are themselves named wrapper/harness helpers (e.g. `g_agent.rs`)
are compiled by the `zz_merged_*` harness that includes them; the filter runs their
own tests (and, for a wrapper, its `suites/` submodules as well).

| # | root test file | harness target | nextest filter |
|---:|---|---|---|
| 1 | `tests/agent_benchmark.rs` | `zz_merged_1` | `-E 'test(agent_benchmark::)'` |
| 2 | `tests/agent_bootstrap.rs` | `zz_merged_1` | `-E 'test(agent_bootstrap::)'` |
| 3 | `tests/agent_end_shares_result.rs` | `zz_merged_1` | `-E 'test(agent_end_shares_result::)'` |
| 4 | `tests/agent_mutation_serialization_fix.rs` | `zz_merged_1` | `-E 'test(agent_mutation_serialization_fix::)'` |
| 5 | `tests/ai_lane_g_w17.rs` | `zz_merged_1` | `-E 'test(ai_lane_g_w17::)'` |
| 6 | `tests/ai_proxy.rs` | `zz_merged_1` | `-E 'test(ai_proxy::)'` |
| 7 | `tests/ai_transport_seam.rs` | `zz_merged_1` | `-E 'test(ai_transport_seam::)'` |
| 8 | `tests/altscreen_parity.rs` | `zz_merged_1` | `-E 'test(altscreen_parity::)'` |
| 9 | `tests/bash_eviction_utf8.rs` | `zz_merged_1` | `-E 'test(bash_eviction_utf8::)'` |
| 10 | `tests/bcrypt_shims.rs` | `zz_merged_1` | `-E 'test(bcrypt_shims::)'` |
| 11 | `tests/child_process_stdio.rs` | `zz_merged_1` | `-E 'test(child_process_stdio::)'` |
| 12 | `tests/client_rpc.rs` | `zz_merged_1` | `-E 'test(client_rpc::)'` |
| 13 | `tests/codemode_exposure.rs` | `zz_merged_1` | `-E 'test(codemode_exposure::)'` |
| 14 | `tests/codemode_worker_cancel.rs` | `zz_merged_1` | `-E 'test(codemode_worker_cancel::)'` |
| 15 | `tests/d32_resume_active_tools.rs` | `zz_merged_1` | `-E 'test(d32_resume_active_tools::)'` |
| 16 | `tests/dry_convergence.rs` | `zz_merged_1` | `-E 'test(dry_convergence::)'` |
| 17 | `tests/durable_shim.rs` | `zz_merged_1` | `-E 'test(durable_shim::)'` |
| 18 | `tests/durable_storage.rs` | `zz_merged_1` | `-E 'test(durable_storage::)'` |
| 19 | `tests/durable_trace_propagation.rs` | `zz_merged_1` | `-E 'test(durable_trace_propagation::)'` |
| 20 | `tests/ecosystem_compat.rs` | `zz_merged_1` | `-E 'test(ecosystem_compat::)'` |
| 21 | `tests/env_client_pty.rs` | `zz_merged_1` | `-E 'test(env_client_pty::)'` |
| 22 | `tests/feature_off_surface.rs` | `zz_merged_1` | `-E 'test(feature_off_surface::)'` |
| 23 | `tests/footer_provider_count.rs` | `zz_merged_1` | `-E 'test(footer_provider_count::)'` |
| 24 | `tests/hook_coverage.rs` | `zz_merged_1` | `-E 'test(hook_coverage::)'` |
| 25 | `tests/hook_timeout.rs` | `zz_merged_1` | `-E 'test(hook_timeout::)'` |
| 26 | `tests/http_async_resource_context.rs` | `zz_merged_1` | `-E 'test(http_async_resource_context::)'` |
| 27 | `tests/http_body_stream_chunks.rs` | `zz_merged_1` | `-E 'test(http_body_stream_chunks::)'` |
| 28 | `tests/image_attachment_resize.rs` | `zz_merged_1` | `-E 'test(image_attachment_resize::)'` |
| 29 | `tests/lane_event2_project_trust.rs` | `zz_merged_1` | `-E 'test(lane_event2_project_trust::)'` |
| 30 | `tests/lane_event3_runtime_handoff.rs` | `zz_merged_1` | `-E 'test(lane_event3_runtime_handoff::)'` |
| 31 | `tests/mcp_builtin_tools.rs` | `zz_merged_1` | `-E 'test(mcp_builtin_tools::)'` |
| 32 | `tests/mcp_integration.rs` | `zz_merged_1` | `-E 'test(mcp_integration::)'` |
| 33 | `tests/node_tests_wave_b.rs` | `zz_merged_1` | `-E 'test(node_tests_wave_b::)'` |
| 34 | `tests/oauth_tui_login.rs` | `zz_merged_1` | `-E 'test(oauth_tui_login::)'` |
| 35 | `tests/p0_jobs.rs` | `zz_merged_1` | `-E 'test(p0_jobs::)'` |
| 36 | `tests/p0_token_estimator.rs` | `zz_merged_1` | `-E 'test(p0_token_estimator::)'` |
| 37 | `tests/phone_builtin_replaceable.rs` | `zz_merged_1` | `-E 'test(phone_builtin_replaceable::)'` |
| 38 | `tests/pi_codemode_host_route.rs` | `zz_merged_1` | `-E 'test(pi_codemode_host_route::)'` |
| 39 | `tests/pi_phone_default_disabled.rs` | `zz_merged_1` | `-E 'test(pi_phone_default_disabled::)'` |
| 40 | `tests/plugin_renderer_completion.rs` | `zz_merged_1` | `-E 'test(plugin_renderer_completion::)'` |
| 41 | `tests/print_lifecycle_hooks.rs` | `zz_merged_1` | `-E 'test(print_lifecycle_hooks::)'` |
| 42 | `tests/python_plugin.rs` | `zz_merged_1` | `-E 'test(python_plugin::)'` |
| 43 | `tests/sdk_agent_session_errors.rs` | `zz_merged_1` | `-E 'test(sdk_agent_session_errors::)'` |
| 44 | `tests/sdk_extended.rs` | `zz_merged_1` | `-E 'test(sdk_extended::)'` |
| 45 | `tests/session_build_context.rs` | `zz_merged_1` | `-E 'test(session_build_context::)'` |
| 46 | `tests/session_context_snapshot_cache.rs` | `zz_merged_1` | `-E 'test(session_context_snapshot_cache::)'` |
| 47 | `tests/shell_pool_prewarm.rs` | `zz_merged_1` | `-E 'test(shell_pool_prewarm::)'` |
| 48 | `tests/subagent_depth.rs` | `zz_merged_1` | `-E 'test(subagent_depth::)'` |
| 49 | `tests/trust_p0_lane_f.rs` | `zz_merged_1` | `-E 'test(trust_p0_lane_f::)'` |
| 50 | `tests/tui_autocomplete_dynamic.rs` | `zz_merged_1` | `-E 'test(tui_autocomplete_dynamic::)'` |
| 51 | `tests/tui_capability_overrides.rs` | `zz_merged_1` | `-E 'test(tui_capability_overrides::)'` |
| 52 | `tests/tui_crash_log_attribution.rs` | `zz_merged_1` | `-E 'test(tui_crash_log_attribution::)'` |
| 53 | `tests/tui_debug_copy_search_w17.rs` | `zz_merged_1` | `-E 'test(tui_debug_copy_search_w17::)'` |
| 54 | `tests/tui_dirty_refresh.rs` | `zz_merged_1` | `-E 'test(tui_dirty_refresh::)'` |
| 55 | `tests/tui_emoji_input.rs` | `zz_merged_1` | `-E 'test(tui_emoji_input::)'` |
| 56 | `tests/tui_escape_sanitize.rs` | `zz_merged_1` | `-E 'test(tui_escape_sanitize::)'` |
| 57 | `tests/tui_input_order.rs` | `zz_merged_1` | `-E 'test(tui_input_order::)'` |
| 58 | `tests/tui_keybinding_namespace.rs` | `zz_merged_1` | `-E 'test(tui_keybinding_namespace::)'` |
| 59 | `tests/tui_mcp_stack_overflow.rs` | `zz_merged_1` | `-E 'test(tui_mcp_stack_overflow::)'` |
| 60 | `tests/tui_paged_load_more.rs` | `zz_merged_1` | `-E 'test(tui_paged_load_more::)'` |
| 61 | `tests/tui_paste_burst_p5.rs` | `zz_merged_1` | `-E 'test(tui_paste_burst_p5::)'` |
| 62 | `tests/tui_session_reset.rs` | `zz_merged_1` | `-E 'test(tui_session_reset::)'` |
| 63 | `tests/upstream_env_protocol.rs` | `zz_merged_1` | `-E 'test(upstream_env_protocol::)'` |
| 64 | `tests/w1_d7_tilde_patterns.rs` | `zz_merged_1` | `-E 'test(w1_d7_tilde_patterns::)'` |
| 65 | `tests/w1_d8_mcp.rs` | `zz_merged_1` | `-E 'test(w1_d8_mcp::)'` |
| 66 | `tests/zlib_crypto_shims.rs` | `zz_merged_1` | `-E 'test(zlib_crypto_shims::)'` |
| 67 | `tests/abort_wakeup.rs` | `zz_merged_2` | `-E 'test(abort_wakeup::)'` |
| 68 | `tests/agent_scheduler_hot_path.rs` | `zz_merged_2` | `-E 'test(agent_scheduler_hot_path::)'` |
| 69 | `tests/agents_skills_discovery.rs` | `zz_merged_2` | `-E 'test(agents_skills_discovery::)'` |
| 70 | `tests/ai_payload_body_reuse.rs` | `zz_merged_2` | `-E 'test(ai_payload_body_reuse::)'` |
| 71 | `tests/child_process_stream_encoding.rs` | `zz_merged_2` | `-E 'test(child_process_stream_encoding::)'` |
| 72 | `tests/chord_shim.rs` | `zz_merged_2` | `-E 'test(chord_shim::)'` |
| 73 | `tests/claude_env_compat.rs` | `zz_merged_2` | `-E 'test(claude_env_compat::)'` |
| 74 | `tests/client_server.rs` | `zz_merged_2` | `-E 'test(client_server::)'` |
| 75 | `tests/coding_agent_shims.rs` | `zz_merged_2` | `-E 'test(coding_agent_shims::)'` |
| 76 | `tests/core_round2_fixes.rs` | `zz_merged_2` | `-E 'test(core_round2_fixes::)'` |
| 77 | `tests/d30_anthropic_error_tool_result_images.rs` | `zz_merged_2` | `-E 'test(d30_anthropic_error_tool_result_images::)'` |
| 78 | `tests/e2e_qwen38max.rs` | `zz_merged_2` | `-E 'test(e2e_qwen38max::)'` |
| 79 | `tests/env_client_e2e.rs` | `zz_merged_2` | `-E 'test(env_client_e2e::)'` |
| 80 | `tests/extension_dispatch_timeout.rs` | `zz_merged_2` | `-E 'test(extension_dispatch_timeout::)'` |
| 81 | `tests/extensions_longtail.rs` | `zz_merged_2` | `-E 'test(extensions_longtail::)'` |
| 82 | `tests/fs_watch_adaptive.rs` | `zz_merged_2` | `-E 'test(fs_watch_adaptive::)'` |
| 83 | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::)'` |
| 84 | `tests/image_classifier_models.rs` | `zz_merged_2` | `-E 'test(image_classifier_models::)'` |
| 85 | `tests/image_pipeline_fix.rs` | `zz_merged_2` | `-E 'test(image_pipeline_fix::)'` |
| 86 | `tests/json_stream_wire.rs` | `zz_merged_2` | `-E 'test(json_stream_wire::)'` |
| 87 | `tests/low_resource_units.rs` | `zz_merged_2` | `-E 'test(low_resource_units::)'` |
| 88 | `tests/mcp_http_body_cap.rs` | `zz_merged_2` | `-E 'test(mcp_http_body_cap::)'` |
| 89 | `tests/mcp_units.rs` | `zz_merged_2` | `-E 'test(mcp_units::)'` |
| 90 | `tests/message_serde_fix.rs` | `zz_merged_2` | `-E 'test(message_serde_fix::)'` |
| 91 | `tests/node_shims.rs` | `zz_merged_2` | `-E 'test(node_shims::)'` |
| 92 | `tests/oauth_page_and_load.rs` | `zz_merged_2` | `-E 'test(oauth_page_and_load::)'` |
| 93 | `tests/osc11_unix_reply_reader.rs` | `zz_merged_2` | `-E 'test(osc11_unix_reply_reader::)'` |
| 94 | `tests/p0_session_retention.rs` | `zz_merged_2` | `-E 'test(p0_session_retention::)'` |
| 95 | `tests/parallel_tool_streaming.rs` | `zz_merged_2` | `-E 'test(parallel_tool_streaming::)'` |
| 96 | `tests/phone_session_unit.rs` | `zz_merged_2` | `-E 'test(phone_session_unit::)'` |
| 97 | `tests/pi_messages_stream_fix.rs` | `zz_merged_2` | `-E 'test(pi_messages_stream_fix::)'` |
| 98 | `tests/plugin_events.rs` | `zz_merged_2` | `-E 'test(plugin_events::)'` |
| 99 | `tests/plugin_fanout_wiring.rs` | `zz_merged_2` | `-E 'test(plugin_fanout_wiring::)'` |
| 100 | `tests/plugin_live_services.rs` | `zz_merged_2` | `-E 'test(plugin_live_services::)'` |
| 101 | `tests/plugin_provider_backend.rs` | `zz_merged_2` | `-E 'test(plugin_provider_backend::)'` |
| 102 | `tests/plugin_services_hardening.rs` | `zz_merged_2` | `-E 'test(plugin_services_hardening::)'` |
| 103 | `tests/plugin_ui_dialogs.rs` | `zz_merged_2` | `-E 'test(plugin_ui_dialogs::)'` |
| 104 | `tests/plugin_widget_factories.rs` | `zz_merged_2` | `-E 'test(plugin_widget_factories::)'` |
| 105 | `tests/provider_catalog_shared.rs` | `zz_merged_2` | `-E 'test(provider_catalog_shared::)'` |
| 106 | `tests/provider_shared_context.rs` | `zz_merged_2` | `-E 'test(provider_shared_context::)'` |
| 107 | `tests/rss_a_entry_append_materialization.rs` | `zz_merged_2` | `-E 'test(rss_a_entry_append_materialization::)'` |
| 108 | `tests/sdk_consumer_lane_f.rs` | `zz_merged_2` | `-E 'test(sdk_consumer_lane_f::)'` |
| 109 | `tests/session_context_cli.rs` | `zz_merged_2` | `-E 'test(session_context_cli::)'` |
| 110 | `tests/session_duplicate_writer.rs` | `zz_merged_2` | `-E 'test(session_duplicate_writer::)'` |
| 111 | `tests/session_lock_scopes.rs` | `zz_merged_2` | `-E 'test(session_lock_scopes::)'` |
| 112 | `tests/settings_diagnostics.rs` | `zz_merged_2` | `-E 'test(settings_diagnostics::)'` |
| 113 | `tests/sqlite_shims.rs` | `zz_merged_2` | `-E 'test(sqlite_shims::)'` |
| 114 | `tests/stream_shim_contract.rs` | `zz_merged_2` | `-E 'test(stream_shim_contract::)'` |
| 115 | `tests/subagent_builtin.rs` | `zz_merged_2` | `-E 'test(subagent_builtin::)'` |
| 116 | `tests/telemetry_ai_cost_w17.rs` | `zz_merged_2` | `-E 'test(telemetry_ai_cost_w17::)'` |
| 117 | `tests/telemetry_store_w17.rs` | `zz_merged_2` | `-E 'test(telemetry_store_w17::)'` |
| 118 | `tests/terminal_input_hooks.rs` | `zz_merged_2` | `-E 'test(terminal_input_hooks::)'` |
| 119 | `tests/theme_once_semantics.rs` | `zz_merged_2` | `-E 'test(theme_once_semantics::)'` |
| 120 | `tests/tui_bugfix_batch.rs` | `zz_merged_2` | `-E 'test(tui_bugfix_batch::)'` |
| 121 | `tests/tui_fixes_round2.rs` | `zz_merged_2` | `-E 'test(tui_fixes_round2::)'` |
| 122 | `tests/tui_frame_reuse.rs` | `zz_merged_2` | `-E 'test(tui_frame_reuse::)'` |
| 123 | `tests/tui_image_preview_off.rs` | `zz_merged_2` | `-E 'test(tui_image_preview_off::)'` |
| 124 | `tests/tui_loop_fixes.rs` | `zz_merged_2` | `-E 'test(tui_loop_fixes::)'` |
| 125 | `tests/tui_search_whole_history.rs` | `zz_merged_2` | `-E 'test(tui_search_whole_history::)'` |
| 126 | `tests/upstream_compaction_summary_limits.rs` | `zz_merged_2` | `-E 'test(upstream_compaction_summary_limits::)'` |
| 127 | `tests/upstream_d36_thinking.rs` | `zz_merged_2` | `-E 'test(upstream_d36_thinking::)'` |
| 128 | `tests/upstream_prompt_templates_args.rs` | `zz_merged_2` | `-E 'test(upstream_prompt_templates_args::)'` |
| 129 | `tests/upstream_w2_drift.rs` | `zz_merged_2` | `-E 'test(upstream_w2_drift::)'` |
| 130 | `tests/w1_d12_openrouter.rs` | `zz_merged_2` | `-E 'test(w1_d12_openrouter::)'` |
| 131 | `tests/widget_factories.rs` | `zz_merged_2` | `-E 'test(widget_factories::)'` |
| 132 | `tests/assert_shim.rs` | `zz_merged_3` | `-E 'test(assert_shim::)'` |
| 133 | `tests/backend_copy_audit.rs` | `zz_merged_3` | `-E 'test(backend_copy_audit::)'` |
| 134 | `tests/bash_tree_kill_prompt.rs` | `zz_merged_3` | `-E 'test(bash_tree_kill_prompt::)'` |
| 135 | `tests/brush_embed_gate.rs` | `zz_merged_3` | `-E 'test(brush_embed_gate::)'` |
| 136 | `tests/catalog_overlay_install.rs` | `zz_merged_3` | `-E 'test(catalog_overlay_install::)'` |
| 137 | `tests/cli.rs` | `zz_merged_3` | `-E 'test(cli::)'` |
| 138 | `tests/client_remove_session.rs` | `zz_merged_3` | `-E 'test(client_remove_session::)'` |
| 139 | `tests/codemode_declarations.rs` | `zz_merged_3` | `-E 'test(codemode_declarations::)'` |
| 140 | `tests/constrained_device_budget.rs` | `zz_merged_3` | `-E 'test(constrained_device_budget::)'` |
| 141 | `tests/context_mode_batch.rs` | `zz_merged_3` | `-E 'test(context_mode_batch::)'` |
| 142 | `tests/context_mode_hardening.rs` | `zz_merged_3` | `-E 'test(context_mode_hardening::)'` |
| 143 | `tests/ctx_actions_and_dialog_timeout.rs` | `zz_merged_3` | `-E 'test(ctx_actions_and_dialog_timeout::)'` |
| 144 | `tests/ctx_ui_parity.rs` | `zz_merged_3` | `-E 'test(ctx_ui_parity::)'` |
| 145 | `tests/d23_reach_consumption.rs` | `zz_merged_3` | `-E 'test(d23_reach_consumption::)'` |
| 146 | `tests/d31_session_share_destination.rs` | `zz_merged_3` | `-E 'test(d31_session_share_destination::)'` |
| 147 | `tests/display_parity.rs` | `zz_merged_3` | `-E 'test(display_parity::)'` |
| 148 | `tests/e2e_cli_agent_session.rs` | `zz_merged_3` | `-E 'test(e2e_cli_agent_session::)'` |
| 149 | `tests/editor_killring_undo.rs` | `zz_merged_3` | `-E 'test(editor_killring_undo::)'` |
| 150 | `tests/env_daemon_protocol.rs` | `zz_merged_3` | `-E 'test(env_daemon_protocol::)'` |
| 151 | `tests/extension_background_preemption.rs` | `zz_merged_3` | `-E 'test(extension_background_preemption::)'` |
| 152 | `tests/extension_fail_cache.rs` | `zz_merged_3` | `-E 'test(extension_fail_cache::)'` |
| 153 | `tests/extensions_runtime.rs` | `zz_merged_3` | `-E 'test(extensions_runtime::)'` |
| 154 | `tests/fs_numeric_open_flags.rs` | `zz_merged_3` | `-E 'test(fs_numeric_open_flags::)'` |
| 155 | `tests/fs_watch_incremental.rs` | `zz_merged_3` | `-E 'test(fs_watch_incremental::)'` |
| 156 | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::)'` |
| 157 | `tests/g_print_tools_w14.rs` | `zz_merged_3` | `-E 'test(g_print_tools_w14::)'` |
| 158 | `tests/lane_event3_json_host.rs` | `zz_merged_3` | `-E 'test(lane_event3_json_host::)'` |
| 159 | `tests/manifest_cache.rs` | `zz_merged_3` | `-E 'test(manifest_cache::)'` |
| 160 | `tests/mcp_builtin_tools_e2e.rs` | `zz_merged_3` | `-E 'test(mcp_builtin_tools_e2e::)'` |
| 161 | `tests/memory_shared.rs` | `zz_merged_3` | `-E 'test(memory_shared::)'` |
| 162 | `tests/minimal_profile.rs` | `zz_merged_3` | `-E 'test(minimal_profile::)'` |
| 163 | `tests/npm_modules.rs` | `zz_merged_3` | `-E 'test(npm_modules::)'` |
| 164 | `tests/openai_model_max_tokens.rs` | `zz_merged_3` | `-E 'test(openai_model_max_tokens::)'` |
| 165 | `tests/optimization_parity.rs` | `zz_merged_3` | `-E 'test(optimization_parity::)'` |
| 166 | `tests/phone_child_inline.rs` | `zz_merged_3` | `-E 'test(phone_child_inline::)'` |
| 167 | `tests/phone_live_session.rs` | `zz_merged_3` | `-E 'test(phone_live_session::)'` |
| 168 | `tests/pi_shims.rs` | `zz_merged_3` | `-E 'test(pi_shims::)'` |
| 169 | `tests/pir_js_runner.rs` | `zz_merged_3` | `-E 'test(pir_js_runner::)'` |
| 170 | `tests/plugin_provider_auth.rs` | `zz_merged_3` | `-E 'test(plugin_provider_auth::)'` |
| 171 | `tests/prompt_path_fixes.rs` | `zz_merged_3` | `-E 'test(prompt_path_fixes::)'` |
| 172 | `tests/provider_config_fixes.rs` | `zz_merged_3` | `-E 'test(provider_config_fixes::)'` |
| 173 | `tests/python_builtin_audit.rs` | `zz_merged_3` | `-E 'test(python_builtin_audit::)'` |
| 174 | `tests/rpc_bash_record_fix.rs` | `zz_merged_3` | `-E 'test(rpc_bash_record_fix::)'` |
| 175 | `tests/rpc_export_off.rs` | `zz_merged_3` | `-E 'test(rpc_export_off::)'` |
| 176 | `tests/sdk_extensibility.rs` | `zz_merged_3` | `-E 'test(sdk_extensibility::)'` |
| 177 | `tests/security_fixes.rs` | `zz_merged_3` | `-E 'test(security_fixes::)'` |
| 178 | `tests/server_cancel_fix.rs` | `zz_merged_3` | `-E 'test(server_cancel_fix::)'` |
| 179 | `tests/server_session_closeout.rs` | `zz_merged_3` | `-E 'test(server_session_closeout::)'` |
| 180 | `tests/session_listing_single_read.rs` | `zz_merged_3` | `-E 'test(session_listing_single_read::)'` |
| 181 | `tests/shell_pool.rs` | `zz_merged_3` | `-E 'test(shell_pool::)'` |
| 182 | `tests/spawn_sync_stdio.rs` | `zz_merged_3` | `-E 'test(spawn_sync_stdio::)'` |
| 183 | `tests/subagent_budget.rs` | `zz_merged_3` | `-E 'test(subagent_budget::)'` |
| 184 | `tests/subagent_cli_parallel.rs` | `zz_merged_3` | `-E 'test(subagent_cli_parallel::)'` |
| 185 | `tests/tool_registration_fix.rs` | `zz_merged_3` | `-E 'test(tool_registration_fix::)'` |
| 186 | `tests/tool_search_declaration.rs` | `zz_merged_3` | `-E 'test(tool_search_declaration::)'` |
| 187 | `tests/truncate_tail_edge.rs` | `zz_merged_3` | `-E 'test(truncate_tail_edge::)'` |
| 188 | `tests/tui_bash_mode.rs` | `zz_merged_3` | `-E 'test(tui_bash_mode::)'` |
| 189 | `tests/tui_command_sources.rs` | `zz_merged_3` | `-E 'test(tui_command_sources::)'` |
| 190 | `tests/tui_commands_parity.rs` | `zz_merged_3` | `-E 'test(tui_commands_parity::)'` |
| 191 | `tests/tui_slash_model_argument.rs` | `zz_merged_3` | `-E 'test(tui_slash_model_argument::)'` |
| 192 | `tests/tui_thinking_footer.rs` | `zz_merged_3` | `-E 'test(tui_thinking_footer::)'` |
| 193 | `tests/tui_toolcall_args_stream.rs` | `zz_merged_3` | `-E 'test(tui_toolcall_args_stream::)'` |
| 194 | `tests/upstream_d3_strict_tool_fallback.rs` | `zz_merged_3` | `-E 'test(upstream_d3_strict_tool_fallback::)'` |
| 195 | `tests/upstream_w1_drift.rs` | `zz_merged_3` | `-E 'test(upstream_w1_drift::)'` |
| 196 | `tests/ai_bedrock_credentials.rs` | `zz_merged_4` | `-E 'test(ai_bedrock_credentials::)'` |
| 197 | `tests/ai_request_snapshots.rs` | `zz_merged_4` | `-E 'test(ai_request_snapshots::)'` |
| 198 | `tests/bash_abort_set.rs` | `zz_merged_4` | `-E 'test(bash_abort_set::)'` |
| 199 | `tests/bedrock_chain_extensions.rs` | `zz_merged_4` | `-E 'test(bedrock_chain_extensions::)'` |
| 200 | `tests/bedrock_p1_fixes.rs` | `zz_merged_4` | `-E 'test(bedrock_p1_fixes::)'` |
| 201 | `tests/brush_embed.rs` | `zz_merged_4` | `-E 'test(brush_embed::)'` |
| 202 | `tests/catalog_cache.rs` | `zz_merged_4` | `-E 'test(catalog_cache::)'` |
| 203 | `tests/cli_parity_fixes.rs` | `zz_merged_4` | `-E 'test(cli_parity_fixes::)'` |
| 204 | `tests/commit_tracker.rs` | `zz_merged_4` | `-E 'test(commit_tracker::)'` |
| 205 | `tests/compaction_evidence.rs` | `zz_merged_4` | `-E 'test(compaction_evidence::)'` |
| 206 | `tests/console_shim.rs` | `zz_merged_4` | `-E 'test(console_shim::)'` |
| 207 | `tests/context_mode.rs` | `zz_merged_4` | `-E 'test(context_mode::)'` |
| 208 | `tests/context_mode_hooks.rs` | `zz_merged_4` | `-E 'test(context_mode_hooks::)'` |
| 209 | `tests/context_mode_index.rs` | `zz_merged_4` | `-E 'test(context_mode_index::)'` |
| 210 | `tests/context_mode_proxy.rs` | `zz_merged_4` | `-E 'test(context_mode_proxy::)'` |
| 211 | `tests/fs_watch_real.rs` | `zz_merged_4` | `-E 'test(fs_watch_real::)'` |
| 212 | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::)'` |
| 213 | `tests/http_poll_pacing.rs` | `zz_merged_4` | `-E 'test(http_poll_pacing::)'` |
| 214 | `tests/http_shims.rs` | `zz_merged_4` | `-E 'test(http_shims::)'` |
| 215 | `tests/mcp_auto_enable.rs` | `zz_merged_4` | `-E 'test(mcp_auto_enable::)'` |
| 216 | `tests/mcp_description_lifecycle.rs` | `zz_merged_4` | `-E 'test(mcp_description_lifecycle::)'` |
| 217 | `tests/mcp_oauth_store.rs` | `zz_merged_4` | `-E 'test(mcp_oauth_store::)'` |
| 218 | `tests/mcp_singleflight_locks.rs` | `zz_merged_4` | `-E 'test(mcp_singleflight_locks::)'` |
| 219 | `tests/mcp_strict.rs` | `zz_merged_4` | `-E 'test(mcp_strict::)'` |
| 220 | `tests/native_cli.rs` | `zz_merged_4` | `-E 'test(native_cli::)'` |
| 221 | `tests/net_shims.rs` | `zz_merged_4` | `-E 'test(net_shims::)'` |
| 222 | `tests/node_shim_injection.rs` | `zz_merged_4` | `-E 'test(node_shim_injection::)'` |
| 223 | `tests/node_surface.rs` | `zz_merged_4` | `-E 'test(node_surface::)'` |
| 224 | `tests/only_builtin_flag.rs` | `zz_merged_4` | `-E 'test(only_builtin_flag::)'` |
| 225 | `tests/openai_responses_parity.rs` | `zz_merged_4` | `-E 'test(openai_responses_parity::)'` |
| 226 | `tests/p0_checkpoints.rs` | `zz_merged_4` | `-E 'test(p0_checkpoints::)'` |
| 227 | `tests/p2_inbox_toctou.rs` | `zz_merged_4` | `-E 'test(p2_inbox_toctou::)'` |
| 228 | `tests/partial_mirror.rs` | `zz_merged_4` | `-E 'test(partial_mirror::)'` |
| 229 | `tests/plan_only_session_stream_cell.rs` | `zz_merged_4` | `-E 'test(plan_only_session_stream_cell::)'` |
| 230 | `tests/plugin_gates.rs` | `zz_merged_4` | `-E 'test(plugin_gates::)'` |
| 231 | `tests/plugin_service_perf.rs` | `zz_merged_4` | `-E 'test(plugin_service_perf::)'` |
| 232 | `tests/plugin_session_actions.rs` | `zz_merged_4` | `-E 'test(plugin_session_actions::)'` |
| 233 | `tests/print_tool_update_json.rs` | `zz_merged_4` | `-E 'test(print_tool_update_json::)'` |
| 234 | `tests/read_before_edit.rs` | `zz_merged_4` | `-E 'test(read_before_edit::)'` |
| 235 | `tests/renderer_parity.rs` | `zz_merged_4` | `-E 'test(renderer_parity::)'` |
| 236 | `tests/renderer_ui_guard.rs` | `zz_merged_4` | `-E 'test(renderer_ui_guard::)'` |
| 237 | `tests/sdk_consumer_embed.rs` | `zz_merged_4` | `-E 'test(sdk_consumer_embed::)'` |
| 238 | `tests/session_append_durability.rs` | `zz_merged_4` | `-E 'test(session_append_durability::)'` |
| 239 | `tests/session_manager_ext.rs` | `zz_merged_4` | `-E 'test(session_manager_ext::)'` |
| 240 | `tests/shell_command_timeout.rs` | `zz_merged_4` | `-E 'test(shell_command_timeout::)'` |
| 241 | `tests/subagent_cli_print.rs` | `zz_merged_4` | `-E 'test(subagent_cli_print::)'` |
| 242 | `tests/subagent_model_catalog.rs` | `zz_merged_4` | `-E 'test(subagent_model_catalog::)'` |
| 243 | `tests/subagent_runtime_isolation.rs` | `zz_merged_4` | `-E 'test(subagent_runtime_isolation::)'` |
| 244 | `tests/subagents_parent_launch.rs` | `zz_merged_4` | `-E 'test(subagents_parent_launch::)'` |
| 245 | `tests/tools_feature_off.rs` | `zz_merged_4` | `-E 'test(tools_feature_off::)'` |
| 246 | `tests/tree_depth_safety.rs` | `zz_merged_4` | `-E 'test(tree_depth_safety::)'` |
| 247 | `tests/tui_100k_frame.rs` | `zz_merged_4` | `-E 'test(tui_100k_frame::)'` |
| 248 | `tests/tui_autocomplete_accept.rs` | `zz_merged_4` | `-E 'test(tui_autocomplete_accept::)'` |
| 249 | `tests/tui_drift_w1.rs` | `zz_merged_4` | `-E 'test(tui_drift_w1::)'` |
| 250 | `tests/tui_emoji_first.rs` | `zz_merged_4` | `-E 'test(tui_emoji_first::)'` |
| 251 | `tests/tui_model_names.rs` | `zz_merged_4` | `-E 'test(tui_model_names::)'` |
| 252 | `tests/tui_native_commands.rs` | `zz_merged_4` | `-E 'test(tui_native_commands::)'` |
| 253 | `tests/tui_quickedit_mode.rs` | `zz_merged_4` | `-E 'test(tui_quickedit_mode::)'` |
| 254 | `tests/tui_render_parity.rs` | `zz_merged_4` | `-E 'test(tui_render_parity::)'` |
| 255 | `tests/tui_session_event_frames.rs` | `zz_merged_4` | `-E 'test(tui_session_event_frames::)'` |
| 256 | `tests/tui_snapshot_refresh_w16.rs` | `zz_merged_4` | `-E 'test(tui_snapshot_refresh_w16::)'` |
| 257 | `tests/tui_windowed_transcript.rs` | `zz_merged_4` | `-E 'test(tui_windowed_transcript::)'` |
| 258 | `tests/unsafe_gate.rs` | `zz_merged_4` | `-E 'test(unsafe_gate::)'` |
| 259 | `tests/version_consistency.rs` | `zz_merged_4` | `-E 'test(version_consistency::)'` |
| 260 | `tests/background_model_tools.rs` | `zz_merged_5` | `-E 'test(background_model_tools::)'` |
| 261 | `tests/builtin_cli_audit.rs` | `zz_merged_5` | `-E 'test(builtin_cli_audit::)'` |
| 262 | `tests/builtin_only_no_js.rs` | `zz_merged_5` | `-E 'test(builtin_only_no_js::)'` |
| 263 | `tests/catalog_google_sync.rs` | `zz_merged_5` | `-E 'test(catalog_google_sync::)'` |
| 264 | `tests/codemode_whitelist.rs` | `zz_merged_5` | `-E 'test(codemode_whitelist::)'` |
| 265 | `tests/diagnostics_channel_shims.rs` | `zz_merged_5` | `-E 'test(diagnostics_channel_shims::)'` |
| 266 | `tests/durable_session_lane_e.rs` | `zz_merged_5` | `-E 'test(durable_session_lane_e::)'` |
| 267 | `tests/ecosystem_runtime_exec.rs` | `zz_merged_5` | `-E 'test(ecosystem_runtime_exec::)'` |
| 268 | `tests/edit_source_limit.rs` | `zz_merged_5` | `-E 'test(edit_source_limit::)'` |
| 269 | `tests/env_daemon_js_shim.rs` | `zz_merged_5` | `-E 'test(env_daemon_js_shim::)'` |
| 270 | `tests/extension_timing.rs` | `zz_merged_5` | `-E 'test(extension_timing::)'` |
| 271 | `tests/extensions_relative_entry.rs` | `zz_merged_5` | `-E 'test(extensions_relative_entry::)'` |
| 272 | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::)'` |
| 273 | `tests/git_branch_head_file.rs` | `zz_merged_5` | `-E 'test(git_branch_head_file::)'` |
| 274 | `tests/grep_context_multi_match.rs` | `zz_merged_5` | `-E 'test(grep_context_multi_match::)'` |
| 275 | `tests/idiom_round3.rs` | `zz_merged_5` | `-E 'test(idiom_round3::)'` |
| 276 | `tests/image_resize_perf.rs` | `zz_merged_5` | `-E 'test(image_resize_perf::)'` |
| 277 | `tests/js_stream_incremental.rs` | `zz_merged_5` | `-E 'test(js_stream_incremental::)'` |
| 278 | `tests/lane_e_streamed_widen.rs` | `zz_merged_5` | `-E 'test(lane_e_streamed_widen::)'` |
| 279 | `tests/lazy_history.rs` | `zz_merged_5` | `-E 'test(lazy_history::)'` |
| 280 | `tests/loader_module_resolution.rs` | `zz_merged_5` | `-E 'test(loader_module_resolution::)'` |
| 281 | `tests/mcp_frame_cap.rs` | `zz_merged_5` | `-E 'test(mcp_frame_cap::)'` |
| 282 | `tests/p2_fixes.rs` | `zz_merged_5` | `-E 'test(p2_fixes::)'` |
| 283 | `tests/paged_session_consumers.rs` | `zz_merged_5` | `-E 'test(paged_session_consumers::)'` |
| 284 | `tests/phone_session_bridge.rs` | `zz_merged_5` | `-E 'test(phone_session_bridge::)'` |
| 285 | `tests/plugin_context.rs` | `zz_merged_5` | `-E 'test(plugin_context::)'` |
| 286 | `tests/plugin_host_statuses.rs` | `zz_merged_5` | `-E 'test(plugin_host_statuses::)'` |
| 287 | `tests/plugin_message_tool_ui.rs` | `zz_merged_5` | `-E 'test(plugin_message_tool_ui::)'` |
| 288 | `tests/plugins_phone_hardening.rs` | `zz_merged_5` | `-E 'test(plugins_phone_hardening::)'` |
| 289 | `tests/print_tool_call_wait.rs` | `zz_merged_5` | `-E 'test(print_tool_call_wait::)'` |
| 290 | `tests/provider_registration_parity.rs` | `zz_merged_5` | `-E 'test(provider_registration_parity::)'` |
| 291 | `tests/python_json_fastpath.rs` | `zz_merged_5` | `-E 'test(python_json_fastpath::)'` |
| 292 | `tests/render_diff.rs` | `zz_merged_5` | `-E 'test(render_diff::)'` |
| 293 | `tests/renderer_in_runtime_fix.rs` | `zz_merged_5` | `-E 'test(renderer_in_runtime_fix::)'` |
| 294 | `tests/rss_b_incremental_projection.rs` | `zz_merged_5` | `-E 'test(rss_b_incremental_projection::)'` |
| 295 | `tests/sdk_consumer_surface.rs` | `zz_merged_5` | `-E 'test(sdk_consumer_surface::)'` |
| 296 | `tests/session_stats_virtual_round_trip.rs` | `zz_merged_5` | `-E 'test(session_stats_virtual_round_trip::)'` |
| 297 | `tests/shell_totals_fix.rs` | `zz_merged_5` | `-E 'test(shell_totals_fix::)'` |
| 298 | `tests/slash_and_scope_parity.rs` | `zz_merged_5` | `-E 'test(slash_and_scope_parity::)'` |
| 299 | `tests/spill_file_cap.rs` | `zz_merged_5` | `-E 'test(spill_file_cap::)'` |
| 300 | `tests/stream_persist_fixes.rs` | `zz_merged_5` | `-E 'test(stream_persist_fixes::)'` |
| 301 | `tests/telemetry_cache.rs` | `zz_merged_5` | `-E 'test(telemetry_cache::)'` |
| 302 | `tests/tool_event_json_payloads.rs` | `zz_merged_5` | `-E 'test(tool_event_json_payloads::)'` |
| 303 | `tests/tui_audit_lane_f.rs` | `zz_merged_5` | `-E 'test(tui_audit_lane_f::)'` |
| 304 | `tests/tui_ime_regression.rs` | `zz_merged_5` | `-E 'test(tui_ime_regression::)'` |
| 305 | `tests/tui_model_selector_configured.rs` | `zz_merged_5` | `-E 'test(tui_model_selector_configured::)'` |
| 306 | `tests/tui_paste_perf.rs` | `zz_merged_5` | `-E 'test(tui_paste_perf::)'` |
| 307 | `tests/tui_shims.rs` | `zz_merged_5` | `-E 'test(tui_shims::)'` |
| 308 | `tests/tui_thinking_persist.rs` | `zz_merged_5` | `-E 'test(tui_thinking_persist::)'` |
| 309 | `tests/upstream_d34_retry.rs` | `zz_merged_5` | `-E 'test(upstream_d34_retry::)'` |
| 310 | `tests/upstream_d35_tool_output.rs` | `zz_merged_5` | `-E 'test(upstream_d35_tool_output::)'` |
| 311 | `tests/web_globals.rs` | `zz_merged_5` | `-E 'test(web_globals::)'` |
| 312 | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::)'` |
| 313 | `tests/lazy_history_memory.rs` | `zz_merged_6` | `-E 'test(lazy_history_memory::)'` |
| 314 | `tests/bash_full_output_bounded.rs` | `zz_merged_7` | `-E 'test(bash_full_output_bounded::)'` |
| 315 | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::)'` |
| 316 | `tests/ai_shims.rs` | `zz_merged_8` | `-E 'test(ai_shims::)'` |
| 317 | `tests/async_hooks_shims.rs` | `zz_merged_8` | `-E 'test(async_hooks_shims::)'` |
| 318 | `tests/auth_storage_async.rs` | `zz_merged_8` | `-E 'test(auth_storage_async::)'` |
| 319 | `tests/differential_node.rs` | `zz_merged_8` | `-E 'test(differential_node::)'` |
| 320 | `tests/e2e_ollama.rs` | `zz_merged_8` | `-E 'test(e2e_ollama::)'` |
| 321 | `tests/find_hidden_files.rs` | `zz_merged_8` | `-E 'test(find_hidden_files::)'` |
| 322 | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::)'` |
| 323 | `tests/lane_e_streaming_file_body.rs` | `zz_merged_8` | `-E 'test(lane_e_streaming_file_body::)'` |
| 324 | `tests/process_beforeexit.rs` | `zz_merged_8` | `-E 'test(process_beforeexit::)'` |
| 325 | `tests/session_manager_scalar_accessors.rs` | `zz_merged_8` | `-E 'test(session_manager_scalar_accessors::)'` |
| 326 | `tests/tool_registry_fingerprint.rs` | `zz_merged_8` | `-E 'test(tool_registry_fingerprint::)'` |
| 327 | `tests/zz_shadow_compaction_peak_attribution.rs` | `zz_merged_8` | `-E 'test(compaction_peak_attribution::)'` |
| 328 | `tests/zz_shadow_lane_e_streamed_peak.rs` | `zz_merged_8` | `-E 'test(lane_e_streamed_peak::)'` |
| 329 | `tests/zz_shadow_lane_e_widen_attribution.rs` | `zz_merged_8` | `-E 'test(lane_e_widen_attribution::)'` |
| 330 | `tests/zz_shadow_sdk_heap_profile.rs` | `zz_merged_8` | `-E 'test(sdk_heap_profile::)'` |
| 331 | `tests/zz_shadow_session_projection_heap.rs` | `zz_merged_8` | `-E 'test(session_projection_heap::)'` |
| 332 | `tests/zz_shadow_startup_heap_profile.rs` | `zz_merged_8` | `-E 'test(startup_heap_profile::)'` |
| 333 | `tests/python_session_lifecycle.rs` | `python_session_lifecycle` | `(whole target; tests are at crate root)` |
| 334 | `tests/worker_threads_real.rs` | `worker_threads_real` | `(whole target; tests are at crate root)` |
| 335 | `tests/durable_memcopy_bench.rs` | `durable_memcopy_bench` | `(whole target; tests are at crate root)` |
| 336 | `tests/g_durable.rs` | `g_durable` | `(whole target; tests are at crate root)` |
| 337 | `tests/print_native_session.rs` | `print_native_session` | `(whole target; tests are at crate root)` |

### Not compiled by any target (shadowed sources)

These six originals declare their own `#[global_allocator]`; the generator emits
`tests/zz_shadow_<stem>.rs` copies (allocator stripped) that `zz_merged_8` builds
instead. The originals are intentionally left in the tree but are not targets.

| shadowed source | built instead as | harness target |
|---|---|---|
| `tests/compaction_peak_attribution.rs` | `tests/zz_shadow_compaction_peak_attribution.rs` | `zz_merged_8` |
| `tests/lane_e_streamed_peak.rs` | `tests/zz_shadow_lane_e_streamed_peak.rs` | `zz_merged_8` |
| `tests/lane_e_widen_attribution.rs` | `tests/zz_shadow_lane_e_widen_attribution.rs` | `zz_merged_8` |
| `tests/sdk_heap_profile.rs` | `tests/zz_shadow_sdk_heap_profile.rs` | `zz_merged_8` |
| `tests/session_projection_heap.rs` | `tests/zz_shadow_session_projection_heap.rs` | `zz_merged_8` |
| `tests/startup_heap_profile.rs` | `tests/zz_shadow_startup_heap_profile.rs` | `zz_merged_8` |

### Composite sources pulled in with `include!`

One compiled source is neither a suite nor a root `mod` include:
`tests/fragments/sdk_round13_on_event.rs` is textually pulled into
`tests/sdk_extended.rs` with `include!("fragments/sdk_round13_on_event.rs")`, so
its tests compile into `zz_merged_1` under the `sdk_extended::` prefix. It is
deliberately not a target of its own (`autotests = false`) and does not appear
in the `#[path = ...]` greps below.

## Verification evidence

Every row above resolves to one of these two greps (plus the `[[test]]` block).
`scripts/which_harness.sh` performs the same lookup at runtime.

### 1. Root files are included by the generated harnesses

```console
$ grep -nF '#[path = ' tests/zz_merged_*.rs
tests/zz_merged_1.rs:15:#[path = "agent_benchmark.rs"]
tests/zz_merged_1.rs:17:#[path = "agent_bootstrap.rs"]
tests/zz_merged_1.rs:19:#[path = "agent_end_shares_result.rs"]
tests/zz_merged_1.rs:21:#[path = "agent_mutation_serialization_fix.rs"]
tests/zz_merged_1.rs:23:#[path = "ai_lane_g_w17.rs"]
tests/zz_merged_1.rs:25:#[path = "ai_proxy.rs"]
tests/zz_merged_1.rs:27:#[path = "ai_transport_seam.rs"]
tests/zz_merged_1.rs:29:#[path = "altscreen_parity.rs"]
tests/zz_merged_1.rs:31:#[path = "bash_eviction_utf8.rs"]
tests/zz_merged_1.rs:33:#[path = "bcrypt_shims.rs"]
tests/zz_merged_1.rs:35:#[path = "child_process_stdio.rs"]
tests/zz_merged_1.rs:37:#[path = "client_rpc.rs"]
tests/zz_merged_1.rs:39:#[path = "codemode_exposure.rs"]
tests/zz_merged_1.rs:41:#[path = "codemode_worker_cancel.rs"]
tests/zz_merged_1.rs:43:#[path = "d32_resume_active_tools.rs"]
tests/zz_merged_1.rs:45:#[path = "dry_convergence.rs"]
tests/zz_merged_1.rs:47:#[path = "durable_shim.rs"]
tests/zz_merged_1.rs:49:#[path = "durable_storage.rs"]
tests/zz_merged_1.rs:51:#[path = "durable_trace_propagation.rs"]
tests/zz_merged_1.rs:53:#[path = "ecosystem_compat.rs"]
tests/zz_merged_1.rs:55:#[path = "env_client_pty.rs"]
tests/zz_merged_1.rs:57:#[path = "feature_off_surface.rs"]
tests/zz_merged_1.rs:59:#[path = "footer_provider_count.rs"]
tests/zz_merged_1.rs:61:#[path = "hook_coverage.rs"]
tests/zz_merged_1.rs:63:#[path = "hook_timeout.rs"]
tests/zz_merged_1.rs:65:#[path = "http_async_resource_context.rs"]
tests/zz_merged_1.rs:67:#[path = "http_body_stream_chunks.rs"]
tests/zz_merged_1.rs:69:#[path = "image_attachment_resize.rs"]
tests/zz_merged_1.rs:71:#[path = "lane_event2_project_trust.rs"]
tests/zz_merged_1.rs:73:#[path = "lane_event3_runtime_handoff.rs"]
tests/zz_merged_1.rs:75:#[path = "mcp_builtin_tools.rs"]
tests/zz_merged_1.rs:77:#[path = "mcp_integration.rs"]
tests/zz_merged_1.rs:79:#[path = "node_tests_wave_b.rs"]
tests/zz_merged_1.rs:81:#[path = "oauth_tui_login.rs"]
tests/zz_merged_1.rs:83:#[path = "p0_jobs.rs"]
tests/zz_merged_1.rs:85:#[path = "p0_token_estimator.rs"]
tests/zz_merged_1.rs:87:#[path = "phone_builtin_replaceable.rs"]
tests/zz_merged_1.rs:89:#[path = "pi_codemode_host_route.rs"]
tests/zz_merged_1.rs:91:#[path = "pi_phone_default_disabled.rs"]
tests/zz_merged_1.rs:93:#[path = "plugin_renderer_completion.rs"]
tests/zz_merged_1.rs:95:#[path = "print_lifecycle_hooks.rs"]
tests/zz_merged_1.rs:97:#[path = "python_plugin.rs"]
tests/zz_merged_1.rs:99:#[path = "sdk_agent_session_errors.rs"]
tests/zz_merged_1.rs:101:#[path = "sdk_extended.rs"]
tests/zz_merged_1.rs:103:#[path = "session_build_context.rs"]
tests/zz_merged_1.rs:105:#[path = "session_context_snapshot_cache.rs"]
tests/zz_merged_1.rs:107:#[path = "shell_pool_prewarm.rs"]
tests/zz_merged_1.rs:109:#[path = "subagent_depth.rs"]
tests/zz_merged_1.rs:111:#[path = "trust_p0_lane_f.rs"]
tests/zz_merged_1.rs:113:#[path = "tui_autocomplete_dynamic.rs"]
tests/zz_merged_1.rs:115:#[path = "tui_capability_overrides.rs"]
tests/zz_merged_1.rs:117:#[path = "tui_crash_log_attribution.rs"]
tests/zz_merged_1.rs:119:#[path = "tui_debug_copy_search_w17.rs"]
tests/zz_merged_1.rs:121:#[path = "tui_dirty_refresh.rs"]
tests/zz_merged_1.rs:123:#[path = "tui_emoji_input.rs"]
tests/zz_merged_1.rs:125:#[path = "tui_escape_sanitize.rs"]
tests/zz_merged_1.rs:127:#[path = "tui_input_order.rs"]
tests/zz_merged_1.rs:129:#[path = "tui_keybinding_namespace.rs"]
tests/zz_merged_1.rs:131:#[path = "tui_mcp_stack_overflow.rs"]
tests/zz_merged_1.rs:133:#[path = "tui_paged_load_more.rs"]
tests/zz_merged_1.rs:135:#[path = "tui_paste_burst_p5.rs"]
tests/zz_merged_1.rs:137:#[path = "tui_session_reset.rs"]
tests/zz_merged_1.rs:139:#[path = "upstream_env_protocol.rs"]
tests/zz_merged_1.rs:141:#[path = "w1_d7_tilde_patterns.rs"]
tests/zz_merged_1.rs:143:#[path = "w1_d8_mcp.rs"]
tests/zz_merged_1.rs:145:#[path = "zlib_crypto_shims.rs"]
tests/zz_merged_2.rs:15:#[path = "abort_wakeup.rs"]
tests/zz_merged_2.rs:17:#[path = "agent_scheduler_hot_path.rs"]
tests/zz_merged_2.rs:19:#[path = "agents_skills_discovery.rs"]
tests/zz_merged_2.rs:21:#[path = "ai_payload_body_reuse.rs"]
tests/zz_merged_2.rs:23:#[path = "child_process_stream_encoding.rs"]
tests/zz_merged_2.rs:25:#[path = "chord_shim.rs"]
tests/zz_merged_2.rs:27:#[path = "claude_env_compat.rs"]
tests/zz_merged_2.rs:29:#[path = "client_server.rs"]
tests/zz_merged_2.rs:31:#[path = "coding_agent_shims.rs"]
tests/zz_merged_2.rs:33:#[path = "core_round2_fixes.rs"]
tests/zz_merged_2.rs:35:#[path = "d30_anthropic_error_tool_result_images.rs"]
tests/zz_merged_2.rs:37:#[path = "e2e_qwen38max.rs"]
tests/zz_merged_2.rs:39:#[path = "env_client_e2e.rs"]
tests/zz_merged_2.rs:41:#[path = "extension_dispatch_timeout.rs"]
tests/zz_merged_2.rs:43:#[path = "extensions_longtail.rs"]
tests/zz_merged_2.rs:45:#[path = "fs_watch_adaptive.rs"]
tests/zz_merged_2.rs:47:#[path = "g_protocol.rs"]
tests/zz_merged_2.rs:49:#[path = "image_classifier_models.rs"]
tests/zz_merged_2.rs:51:#[path = "image_pipeline_fix.rs"]
tests/zz_merged_2.rs:53:#[path = "json_stream_wire.rs"]
tests/zz_merged_2.rs:55:#[path = "low_resource_units.rs"]
tests/zz_merged_2.rs:57:#[path = "mcp_http_body_cap.rs"]
tests/zz_merged_2.rs:59:#[path = "mcp_units.rs"]
tests/zz_merged_2.rs:61:#[path = "message_serde_fix.rs"]
tests/zz_merged_2.rs:63:#[path = "node_shims.rs"]
tests/zz_merged_2.rs:65:#[path = "oauth_page_and_load.rs"]
tests/zz_merged_2.rs:67:#[path = "osc11_unix_reply_reader.rs"]
tests/zz_merged_2.rs:69:#[path = "p0_session_retention.rs"]
tests/zz_merged_2.rs:71:#[path = "parallel_tool_streaming.rs"]
tests/zz_merged_2.rs:73:#[path = "phone_session_unit.rs"]
tests/zz_merged_2.rs:75:#[path = "pi_messages_stream_fix.rs"]
tests/zz_merged_2.rs:77:#[path = "plugin_events.rs"]
tests/zz_merged_2.rs:79:#[path = "plugin_fanout_wiring.rs"]
tests/zz_merged_2.rs:81:#[path = "plugin_live_services.rs"]
tests/zz_merged_2.rs:83:#[path = "plugin_provider_backend.rs"]
tests/zz_merged_2.rs:85:#[path = "plugin_services_hardening.rs"]
tests/zz_merged_2.rs:87:#[path = "plugin_ui_dialogs.rs"]
tests/zz_merged_2.rs:89:#[path = "plugin_widget_factories.rs"]
tests/zz_merged_2.rs:91:#[path = "provider_catalog_shared.rs"]
tests/zz_merged_2.rs:93:#[path = "provider_shared_context.rs"]
tests/zz_merged_2.rs:95:#[path = "rss_a_entry_append_materialization.rs"]
tests/zz_merged_2.rs:97:#[path = "sdk_consumer_lane_f.rs"]
tests/zz_merged_2.rs:99:#[path = "session_context_cli.rs"]
tests/zz_merged_2.rs:101:#[path = "session_duplicate_writer.rs"]
tests/zz_merged_2.rs:103:#[path = "session_lock_scopes.rs"]
tests/zz_merged_2.rs:105:#[path = "settings_diagnostics.rs"]
tests/zz_merged_2.rs:107:#[path = "sqlite_shims.rs"]
tests/zz_merged_2.rs:109:#[path = "stream_shim_contract.rs"]
tests/zz_merged_2.rs:111:#[path = "subagent_builtin.rs"]
tests/zz_merged_2.rs:113:#[path = "telemetry_ai_cost_w17.rs"]
tests/zz_merged_2.rs:115:#[path = "telemetry_store_w17.rs"]
tests/zz_merged_2.rs:117:#[path = "terminal_input_hooks.rs"]
tests/zz_merged_2.rs:119:#[path = "theme_once_semantics.rs"]
tests/zz_merged_2.rs:121:#[path = "tui_bugfix_batch.rs"]
tests/zz_merged_2.rs:123:#[path = "tui_fixes_round2.rs"]
tests/zz_merged_2.rs:125:#[path = "tui_frame_reuse.rs"]
tests/zz_merged_2.rs:127:#[path = "tui_image_preview_off.rs"]
tests/zz_merged_2.rs:129:#[path = "tui_loop_fixes.rs"]
tests/zz_merged_2.rs:131:#[path = "tui_search_whole_history.rs"]
tests/zz_merged_2.rs:133:#[path = "upstream_compaction_summary_limits.rs"]
tests/zz_merged_2.rs:135:#[path = "upstream_d36_thinking.rs"]
tests/zz_merged_2.rs:137:#[path = "upstream_prompt_templates_args.rs"]
tests/zz_merged_2.rs:139:#[path = "upstream_w2_drift.rs"]
tests/zz_merged_2.rs:141:#[path = "w1_d12_openrouter.rs"]
tests/zz_merged_2.rs:143:#[path = "widget_factories.rs"]
tests/zz_merged_3.rs:15:#[path = "assert_shim.rs"]
tests/zz_merged_3.rs:17:#[path = "backend_copy_audit.rs"]
tests/zz_merged_3.rs:19:#[path = "bash_tree_kill_prompt.rs"]
tests/zz_merged_3.rs:21:#[path = "brush_embed_gate.rs"]
tests/zz_merged_3.rs:23:#[path = "catalog_overlay_install.rs"]
tests/zz_merged_3.rs:25:#[path = "cli.rs"]
tests/zz_merged_3.rs:27:#[path = "client_remove_session.rs"]
tests/zz_merged_3.rs:29:#[path = "codemode_declarations.rs"]
tests/zz_merged_3.rs:31:#[path = "constrained_device_budget.rs"]
tests/zz_merged_3.rs:33:#[path = "context_mode_batch.rs"]
tests/zz_merged_3.rs:35:#[path = "context_mode_hardening.rs"]
tests/zz_merged_3.rs:37:#[path = "ctx_actions_and_dialog_timeout.rs"]
tests/zz_merged_3.rs:39:#[path = "ctx_ui_parity.rs"]
tests/zz_merged_3.rs:41:#[path = "d23_reach_consumption.rs"]
tests/zz_merged_3.rs:43:#[path = "d31_session_share_destination.rs"]
tests/zz_merged_3.rs:45:#[path = "display_parity.rs"]
tests/zz_merged_3.rs:47:#[path = "e2e_cli_agent_session.rs"]
tests/zz_merged_3.rs:49:#[path = "editor_killring_undo.rs"]
tests/zz_merged_3.rs:51:#[path = "env_daemon_protocol.rs"]
tests/zz_merged_3.rs:53:#[path = "extension_background_preemption.rs"]
tests/zz_merged_3.rs:55:#[path = "extension_fail_cache.rs"]
tests/zz_merged_3.rs:57:#[path = "extensions_runtime.rs"]
tests/zz_merged_3.rs:59:#[path = "fs_numeric_open_flags.rs"]
tests/zz_merged_3.rs:61:#[path = "fs_watch_incremental.rs"]
tests/zz_merged_3.rs:63:#[path = "g_extensions.rs"]
tests/zz_merged_3.rs:65:#[path = "g_print_tools_w14.rs"]
tests/zz_merged_3.rs:67:#[path = "lane_event3_json_host.rs"]
tests/zz_merged_3.rs:69:#[path = "manifest_cache.rs"]
tests/zz_merged_3.rs:71:#[path = "mcp_builtin_tools_e2e.rs"]
tests/zz_merged_3.rs:73:#[path = "memory_shared.rs"]
tests/zz_merged_3.rs:75:#[path = "minimal_profile.rs"]
tests/zz_merged_3.rs:77:#[path = "npm_modules.rs"]
tests/zz_merged_3.rs:79:#[path = "openai_model_max_tokens.rs"]
tests/zz_merged_3.rs:81:#[path = "optimization_parity.rs"]
tests/zz_merged_3.rs:83:#[path = "phone_child_inline.rs"]
tests/zz_merged_3.rs:85:#[path = "phone_live_session.rs"]
tests/zz_merged_3.rs:87:#[path = "pi_shims.rs"]
tests/zz_merged_3.rs:89:#[path = "pir_js_runner.rs"]
tests/zz_merged_3.rs:91:#[path = "plugin_provider_auth.rs"]
tests/zz_merged_3.rs:93:#[path = "prompt_path_fixes.rs"]
tests/zz_merged_3.rs:95:#[path = "provider_config_fixes.rs"]
tests/zz_merged_3.rs:97:#[path = "python_builtin_audit.rs"]
tests/zz_merged_3.rs:99:#[path = "rpc_bash_record_fix.rs"]
tests/zz_merged_3.rs:101:#[path = "rpc_export_off.rs"]
tests/zz_merged_3.rs:103:#[path = "sdk_extensibility.rs"]
tests/zz_merged_3.rs:105:#[path = "security_fixes.rs"]
tests/zz_merged_3.rs:107:#[path = "server_cancel_fix.rs"]
tests/zz_merged_3.rs:109:#[path = "server_session_closeout.rs"]
tests/zz_merged_3.rs:111:#[path = "session_listing_single_read.rs"]
tests/zz_merged_3.rs:113:#[path = "shell_pool.rs"]
tests/zz_merged_3.rs:115:#[path = "spawn_sync_stdio.rs"]
tests/zz_merged_3.rs:117:#[path = "subagent_budget.rs"]
tests/zz_merged_3.rs:119:#[path = "subagent_cli_parallel.rs"]
tests/zz_merged_3.rs:121:#[path = "tool_registration_fix.rs"]
tests/zz_merged_3.rs:123:#[path = "tool_search_declaration.rs"]
tests/zz_merged_3.rs:125:#[path = "truncate_tail_edge.rs"]
tests/zz_merged_3.rs:127:#[path = "tui_bash_mode.rs"]
tests/zz_merged_3.rs:129:#[path = "tui_command_sources.rs"]
tests/zz_merged_3.rs:131:#[path = "tui_commands_parity.rs"]
tests/zz_merged_3.rs:133:#[path = "tui_slash_model_argument.rs"]
tests/zz_merged_3.rs:135:#[path = "tui_thinking_footer.rs"]
tests/zz_merged_3.rs:137:#[path = "tui_toolcall_args_stream.rs"]
tests/zz_merged_3.rs:139:#[path = "upstream_d3_strict_tool_fallback.rs"]
tests/zz_merged_3.rs:141:#[path = "upstream_w1_drift.rs"]
tests/zz_merged_4.rs:15:#[path = "ai_bedrock_credentials.rs"]
tests/zz_merged_4.rs:17:#[path = "ai_request_snapshots.rs"]
tests/zz_merged_4.rs:19:#[path = "bash_abort_set.rs"]
tests/zz_merged_4.rs:21:#[path = "bedrock_chain_extensions.rs"]
tests/zz_merged_4.rs:23:#[path = "bedrock_p1_fixes.rs"]
tests/zz_merged_4.rs:25:#[path = "brush_embed.rs"]
tests/zz_merged_4.rs:27:#[path = "catalog_cache.rs"]
tests/zz_merged_4.rs:29:#[path = "cli_parity_fixes.rs"]
tests/zz_merged_4.rs:31:#[path = "commit_tracker.rs"]
tests/zz_merged_4.rs:33:#[path = "compaction_evidence.rs"]
tests/zz_merged_4.rs:35:#[path = "console_shim.rs"]
tests/zz_merged_4.rs:37:#[path = "context_mode.rs"]
tests/zz_merged_4.rs:39:#[path = "context_mode_hooks.rs"]
tests/zz_merged_4.rs:41:#[path = "context_mode_index.rs"]
tests/zz_merged_4.rs:43:#[path = "context_mode_proxy.rs"]
tests/zz_merged_4.rs:45:#[path = "fs_watch_real.rs"]
tests/zz_merged_4.rs:47:#[path = "g_cli_server.rs"]
tests/zz_merged_4.rs:49:#[path = "http_poll_pacing.rs"]
tests/zz_merged_4.rs:51:#[path = "http_shims.rs"]
tests/zz_merged_4.rs:53:#[path = "mcp_auto_enable.rs"]
tests/zz_merged_4.rs:55:#[path = "mcp_description_lifecycle.rs"]
tests/zz_merged_4.rs:57:#[path = "mcp_oauth_store.rs"]
tests/zz_merged_4.rs:59:#[path = "mcp_singleflight_locks.rs"]
tests/zz_merged_4.rs:61:#[path = "mcp_strict.rs"]
tests/zz_merged_4.rs:63:#[path = "native_cli.rs"]
tests/zz_merged_4.rs:65:#[path = "net_shims.rs"]
tests/zz_merged_4.rs:67:#[path = "node_shim_injection.rs"]
tests/zz_merged_4.rs:69:#[path = "node_surface.rs"]
tests/zz_merged_4.rs:71:#[path = "only_builtin_flag.rs"]
tests/zz_merged_4.rs:73:#[path = "openai_responses_parity.rs"]
tests/zz_merged_4.rs:75:#[path = "p0_checkpoints.rs"]
tests/zz_merged_4.rs:77:#[path = "p2_inbox_toctou.rs"]
tests/zz_merged_4.rs:79:#[path = "partial_mirror.rs"]
tests/zz_merged_4.rs:81:#[path = "plan_only_session_stream_cell.rs"]
tests/zz_merged_4.rs:83:#[path = "plugin_gates.rs"]
tests/zz_merged_4.rs:85:#[path = "plugin_service_perf.rs"]
tests/zz_merged_4.rs:87:#[path = "plugin_session_actions.rs"]
tests/zz_merged_4.rs:89:#[path = "print_tool_update_json.rs"]
tests/zz_merged_4.rs:91:#[path = "read_before_edit.rs"]
tests/zz_merged_4.rs:93:#[path = "renderer_parity.rs"]
tests/zz_merged_4.rs:95:#[path = "renderer_ui_guard.rs"]
tests/zz_merged_4.rs:97:#[path = "sdk_consumer_embed.rs"]
tests/zz_merged_4.rs:99:#[path = "session_append_durability.rs"]
tests/zz_merged_4.rs:101:#[path = "session_manager_ext.rs"]
tests/zz_merged_4.rs:103:#[path = "shell_command_timeout.rs"]
tests/zz_merged_4.rs:105:#[path = "subagent_cli_print.rs"]
tests/zz_merged_4.rs:107:#[path = "subagent_model_catalog.rs"]
tests/zz_merged_4.rs:109:#[path = "subagent_runtime_isolation.rs"]
tests/zz_merged_4.rs:111:#[path = "subagents_parent_launch.rs"]
tests/zz_merged_4.rs:113:#[path = "tools_feature_off.rs"]
tests/zz_merged_4.rs:115:#[path = "tree_depth_safety.rs"]
tests/zz_merged_4.rs:117:#[path = "tui_100k_frame.rs"]
tests/zz_merged_4.rs:119:#[path = "tui_autocomplete_accept.rs"]
tests/zz_merged_4.rs:121:#[path = "tui_drift_w1.rs"]
tests/zz_merged_4.rs:123:#[path = "tui_emoji_first.rs"]
tests/zz_merged_4.rs:125:#[path = "tui_model_names.rs"]
tests/zz_merged_4.rs:127:#[path = "tui_native_commands.rs"]
tests/zz_merged_4.rs:129:#[path = "tui_quickedit_mode.rs"]
tests/zz_merged_4.rs:131:#[path = "tui_render_parity.rs"]
tests/zz_merged_4.rs:133:#[path = "tui_session_event_frames.rs"]
tests/zz_merged_4.rs:135:#[path = "tui_snapshot_refresh_w16.rs"]
tests/zz_merged_4.rs:137:#[path = "tui_windowed_transcript.rs"]
tests/zz_merged_4.rs:139:#[path = "unsafe_gate.rs"]
tests/zz_merged_4.rs:141:#[path = "version_consistency.rs"]
tests/zz_merged_5.rs:15:#[path = "background_model_tools.rs"]
tests/zz_merged_5.rs:17:#[path = "builtin_cli_audit.rs"]
tests/zz_merged_5.rs:19:#[path = "builtin_only_no_js.rs"]
tests/zz_merged_5.rs:21:#[path = "catalog_google_sync.rs"]
tests/zz_merged_5.rs:23:#[path = "codemode_whitelist.rs"]
tests/zz_merged_5.rs:25:#[path = "diagnostics_channel_shims.rs"]
tests/zz_merged_5.rs:27:#[path = "durable_session_lane_e.rs"]
tests/zz_merged_5.rs:29:#[path = "ecosystem_runtime_exec.rs"]
tests/zz_merged_5.rs:31:#[path = "edit_source_limit.rs"]
tests/zz_merged_5.rs:33:#[path = "env_daemon_js_shim.rs"]
tests/zz_merged_5.rs:35:#[path = "extension_timing.rs"]
tests/zz_merged_5.rs:37:#[path = "extensions_relative_entry.rs"]
tests/zz_merged_5.rs:39:#[path = "g_tools.rs"]
tests/zz_merged_5.rs:41:#[path = "git_branch_head_file.rs"]
tests/zz_merged_5.rs:43:#[path = "grep_context_multi_match.rs"]
tests/zz_merged_5.rs:45:#[path = "idiom_round3.rs"]
tests/zz_merged_5.rs:47:#[path = "image_resize_perf.rs"]
tests/zz_merged_5.rs:49:#[path = "js_stream_incremental.rs"]
tests/zz_merged_5.rs:51:#[path = "lane_e_streamed_widen.rs"]
tests/zz_merged_5.rs:53:#[path = "lazy_history.rs"]
tests/zz_merged_5.rs:55:#[path = "loader_module_resolution.rs"]
tests/zz_merged_5.rs:57:#[path = "mcp_frame_cap.rs"]
tests/zz_merged_5.rs:59:#[path = "p2_fixes.rs"]
tests/zz_merged_5.rs:61:#[path = "paged_session_consumers.rs"]
tests/zz_merged_5.rs:63:#[path = "phone_session_bridge.rs"]
tests/zz_merged_5.rs:65:#[path = "plugin_context.rs"]
tests/zz_merged_5.rs:67:#[path = "plugin_host_statuses.rs"]
tests/zz_merged_5.rs:69:#[path = "plugin_message_tool_ui.rs"]
tests/zz_merged_5.rs:71:#[path = "plugins_phone_hardening.rs"]
tests/zz_merged_5.rs:73:#[path = "print_tool_call_wait.rs"]
tests/zz_merged_5.rs:75:#[path = "provider_registration_parity.rs"]
tests/zz_merged_5.rs:77:#[path = "python_json_fastpath.rs"]
tests/zz_merged_5.rs:79:#[path = "render_diff.rs"]
tests/zz_merged_5.rs:81:#[path = "renderer_in_runtime_fix.rs"]
tests/zz_merged_5.rs:83:#[path = "rss_b_incremental_projection.rs"]
tests/zz_merged_5.rs:85:#[path = "sdk_consumer_surface.rs"]
tests/zz_merged_5.rs:87:#[path = "session_stats_virtual_round_trip.rs"]
tests/zz_merged_5.rs:89:#[path = "shell_totals_fix.rs"]
tests/zz_merged_5.rs:91:#[path = "slash_and_scope_parity.rs"]
tests/zz_merged_5.rs:93:#[path = "spill_file_cap.rs"]
tests/zz_merged_5.rs:95:#[path = "stream_persist_fixes.rs"]
tests/zz_merged_5.rs:97:#[path = "telemetry_cache.rs"]
tests/zz_merged_5.rs:99:#[path = "tool_event_json_payloads.rs"]
tests/zz_merged_5.rs:101:#[path = "tui_audit_lane_f.rs"]
tests/zz_merged_5.rs:103:#[path = "tui_ime_regression.rs"]
tests/zz_merged_5.rs:105:#[path = "tui_model_selector_configured.rs"]
tests/zz_merged_5.rs:107:#[path = "tui_paste_perf.rs"]
tests/zz_merged_5.rs:109:#[path = "tui_shims.rs"]
tests/zz_merged_5.rs:111:#[path = "tui_thinking_persist.rs"]
tests/zz_merged_5.rs:113:#[path = "upstream_d34_retry.rs"]
tests/zz_merged_5.rs:115:#[path = "upstream_d35_tool_output.rs"]
tests/zz_merged_5.rs:117:#[path = "web_globals.rs"]
tests/zz_merged_6.rs:15:#[path = "g_agent.rs"]
tests/zz_merged_6.rs:17:#[path = "lazy_history_memory.rs"]
tests/zz_merged_7.rs:15:#[path = "bash_full_output_bounded.rs"]
tests/zz_merged_7.rs:17:#[path = "g_tui.rs"]
tests/zz_merged_8.rs:21:#[path = "ai_shims.rs"]
tests/zz_merged_8.rs:23:#[path = "async_hooks_shims.rs"]
tests/zz_merged_8.rs:25:#[path = "auth_storage_async.rs"]
tests/zz_merged_8.rs:27:#[path = "zz_shadow_compaction_peak_attribution.rs"]
tests/zz_merged_8.rs:29:#[path = "differential_node.rs"]
tests/zz_merged_8.rs:31:#[path = "e2e_ollama.rs"]
tests/zz_merged_8.rs:33:#[path = "find_hidden_files.rs"]
tests/zz_merged_8.rs:35:#[path = "g_ai.rs"]
tests/zz_merged_8.rs:37:#[path = "zz_shadow_lane_e_streamed_peak.rs"]
tests/zz_merged_8.rs:39:#[path = "lane_e_streaming_file_body.rs"]
tests/zz_merged_8.rs:41:#[path = "zz_shadow_lane_e_widen_attribution.rs"]
tests/zz_merged_8.rs:43:#[path = "process_beforeexit.rs"]
tests/zz_merged_8.rs:45:#[path = "zz_shadow_sdk_heap_profile.rs"]
tests/zz_merged_8.rs:47:#[path = "session_manager_scalar_accessors.rs"]
tests/zz_merged_8.rs:49:#[path = "zz_shadow_session_projection_heap.rs"]
tests/zz_merged_8.rs:51:#[path = "zz_shadow_startup_heap_profile.rs"]
tests/zz_merged_8.rs:53:#[path = "tool_registry_fingerprint.rs"]
```

### 2. `tests/suites/*.rs` files are included by their `tests/*.rs` wrappers

```console
$ grep -rnF '#[path = "suites/' tests/*.rs
tests/agent_bootstrap.rs:7:#[path = "suites/agent_bootstrap.rs"]
tests/g_agent.rs:11:#[path = "suites/agent_core_w11.rs"]
tests/g_agent.rs:13:#[path = "suites/agent_core_w8.rs"]
tests/g_agent.rs:15:#[path = "suites/agent_dependencies.rs"]
tests/g_agent.rs:17:#[path = "suites/agent_dir_identity_w16.rs"]
tests/g_agent.rs:19:#[path = "suites/agent_loop.rs"]
tests/g_agent.rs:21:#[path = "suites/agent_loop_parity.rs"]
tests/g_agent.rs:23:#[path = "suites/agent_loop_units.rs"]
tests/g_agent.rs:25:#[path = "suites/agent_session.rs"]
tests/g_agent.rs:27:#[path = "suites/agent_session_units.rs"]
tests/g_agent.rs:29:#[path = "suites/agent_tool_execution.rs"]
tests/g_agent.rs:31:#[path = "suites/agent_tool_types.rs"]
tests/g_agent.rs:33:#[path = "suites/agent_tool_workflow.rs"]
tests/g_agent.rs:35:#[path = "suites/agent_tools.rs"]
tests/g_agent.rs:37:#[path = "suites/background_tool_calls.rs"]
tests/g_agent.rs:39:#[path = "suites/branch_summarization.rs"]
tests/g_agent.rs:41:#[path = "suites/cache_warmer.rs"]
tests/g_agent.rs:43:#[path = "suites/compaction.rs"]
tests/g_agent.rs:45:#[path = "suites/compaction_chunking.rs"]
tests/g_agent.rs:47:#[path = "suites/context_usage_projection_estimate.rs"]
tests/g_agent.rs:49:#[path = "suites/env_identity_w18.rs"]
tests/g_agent.rs:51:#[path = "suites/hooks_payload_w20.rs"]
tests/g_agent.rs:53:#[path = "suites/hooks_session_w21.rs"]
tests/g_agent.rs:55:#[path = "suites/mcp_agent_dir_w16.rs"]
tests/g_agent.rs:57:#[path = "suites/mcp_env_w11.rs"]
tests/g_agent.rs:59:#[path = "suites/mcp_plugin_root_w17.rs"]
tests/g_agent.rs:61:#[path = "suites/mcp_prune_w18.rs"]
tests/g_agent.rs:63:#[path = "suites/mcp_residuals_w13.rs"]
tests/g_agent.rs:65:#[path = "suites/mcp_signin_text_w16.rs"]
tests/g_agent.rs:67:#[path = "suites/mcp_slash_command_w13.rs"]
tests/g_agent.rs:69:#[path = "suites/mcp_status_w13.rs"]
tests/g_agent.rs:71:#[path = "suites/mcp_toggle_refresh_w18.rs"]
tests/g_agent.rs:73:#[path = "suites/memory_builtin.rs"]
tests/g_agent.rs:75:#[path = "suites/model_switch_auth_w17.rs"]
tests/g_agent.rs:77:#[path = "suites/native_host_w19.rs"]
tests/g_agent.rs:79:#[path = "suites/parallel_concurrency.rs"]
tests/g_agent.rs:81:#[path = "suites/phone_units.rs"]
tests/g_agent.rs:83:#[path = "suites/plugin_events_w16.rs"]
tests/g_agent.rs:85:#[path = "suites/plugin_ledger_w14.rs"]
tests/g_agent.rs:87:#[path = "suites/plugin_services_w10.rs"]
tests/g_agent.rs:89:#[path = "suites/plugins_registry_gate.rs"]
tests/g_agent.rs:91:#[path = "suites/plugins_unified.rs"]
tests/g_agent.rs:93:#[path = "suites/plugins_units.rs"]
tests/g_agent.rs:95:#[path = "suites/plugins_w11.rs"]
tests/g_agent.rs:97:#[path = "suites/print_callability_w13.rs"]
tests/g_agent.rs:99:#[path = "suites/print_mode.rs"]
tests/g_agent.rs:101:#[path = "suites/reload_native_w14.rs"]
tests/g_agent.rs:103:#[path = "suites/resume_system_prompt_once.rs"]
tests/g_agent.rs:105:#[path = "suites/run_diagnostics.rs"]
tests/g_agent.rs:107:#[path = "suites/sdk.rs"]
tests/g_agent.rs:109:#[path = "suites/sdk_agent_isolation_w16.rs"]
tests/g_agent.rs:111:#[path = "suites/sdk_defaults_w13.rs"]
tests/g_agent.rs:113:#[path = "suites/sdk_event_units.rs"]
tests/g_agent.rs:115:#[path = "suites/sdk_gaps_w11.rs"]
tests/g_agent.rs:117:#[path = "suites/sdk_plugins_w8.rs"]
tests/g_agent.rs:119:#[path = "suites/sdk_proxy_w13.rs"]
tests/g_agent.rs:121:#[path = "suites/sdk_units.rs"]
tests/g_agent.rs:123:#[path = "suites/session_commands.rs"]
tests/g_agent.rs:125:#[path = "suites/session_compaction_w11.rs"]
tests/g_agent.rs:127:#[path = "suites/session_ctx_units.rs"]
tests/g_agent.rs:129:#[path = "suites/session_events_parity.rs"]
tests/g_agent.rs:131:#[path = "suites/session_file_contract_w14.rs"]
tests/g_agent.rs:133:#[path = "suites/session_manager.rs"]
tests/g_agent.rs:135:#[path = "suites/session_rewrite_w13.rs"]
tests/g_agent.rs:137:#[path = "suites/startup_thinking_clamp.rs"]
tests/g_agent.rs:139:#[path = "suites/todo_builtin.rs"]
tests/g_ai.rs:4:#[path = "suites/ai_abort_partial_w13.rs"]
tests/g_ai.rs:6:#[path = "suites/ai_abort_signal.rs"]
tests/g_ai.rs:8:#[path = "suites/ai_adapter_fixes.rs"]
tests/g_ai.rs:10:#[path = "suites/ai_adapter_parity.rs"]
tests/g_ai.rs:12:#[path = "suites/ai_adapters_parity.rs"]
tests/g_ai.rs:14:#[path = "suites/ai_adapters_w11.rs"]
tests/g_ai.rs:16:#[path = "suites/ai_auth.rs"]
tests/g_ai.rs:18:#[path = "suites/ai_bedrock_credentials.rs"]
tests/g_ai.rs:20:#[path = "suites/ai_catalog_full_w10.rs"]
tests/g_ai.rs:22:#[path = "suites/ai_catalog_w10.rs"]
tests/g_ai.rs:24:#[path = "suites/ai_codex_ws_w10.rs"]
tests/g_ai.rs:26:#[path = "suites/ai_codex_ws_w13.rs"]
tests/g_ai.rs:28:#[path = "suites/ai_copilot_headers.rs"]
tests/g_ai.rs:30:#[path = "suites/ai_error.rs"]
tests/g_ai.rs:32:#[path = "suites/ai_event_stream.rs"]
tests/g_ai.rs:34:#[path = "suites/ai_event_stream_backpressure.rs"]
tests/g_ai.rs:36:#[path = "suites/ai_event_stream_drop_w13.rs"]
tests/g_ai.rs:38:#[path = "suites/ai_event_stream_ext.rs"]
tests/g_ai.rs:40:#[path = "suites/ai_google.rs"]
tests/g_ai.rs:42:#[path = "suites/ai_http.rs"]
tests/g_ai.rs:44:#[path = "suites/ai_malformed_args_w10.rs"]
tests/g_ai.rs:46:#[path = "suites/ai_pi_messages_cow.rs"]
tests/g_ai.rs:48:#[path = "suites/ai_producer_liveness_w13.rs"]
tests/g_ai.rs:50:#[path = "suites/ai_provider.rs"]
tests/g_ai.rs:52:#[path = "suites/ai_provider_contracts.rs"]
tests/g_ai.rs:54:#[path = "suites/ai_remaining_adapter_contracts.rs"]
tests/g_ai.rs:56:#[path = "suites/ai_responses_tool_changes.rs"]
tests/g_ai.rs:58:#[path = "suites/ai_retry_overflow_d2.rs"]
tests/g_ai.rs:60:#[path = "suites/ai_sse.rs"]
tests/g_ai.rs:62:#[path = "suites/ai_sse_proptest.rs"]
tests/g_ai.rs:64:#[path = "suites/ai_telemetry_w11.rs"]
tests/g_ai.rs:66:#[path = "suites/ai_transcript.rs"]
tests/g_ai.rs:68:#[path = "suites/ai_transport_idle.rs"]
tests/g_ai.rs:70:#[path = "suites/ai_types.rs"]
tests/g_ai.rs:72:#[path = "suites/ai_util.rs"]
tests/g_ai.rs:74:#[path = "suites/auth_cli.rs"]
tests/g_ai.rs:76:#[path = "suites/auth_oauth.rs"]
tests/g_ai.rs:78:#[path = "suites/auth_oauth_unit.rs"]
tests/g_ai.rs:80:#[path = "suites/builtins_cross.rs"]
tests/g_ai.rs:82:#[path = "suites/catalog_drift.rs"]
tests/g_ai.rs:84:#[path = "suites/catalog_units.rs"]
tests/g_ai.rs:88:#[path = "suites/config_cache_lifetime_w16.rs"]
tests/g_ai.rs:90:#[path = "suites/g11_provider_attribution.rs"]
tests/g_ai.rs:92:#[path = "suites/globset_w11.rs"]
tests/g_ai.rs:94:#[path = "suites/model_catalog.rs"]
tests/g_ai.rs:96:#[path = "suites/model_catalog_strict.rs"]
tests/g_ai.rs:98:#[path = "suites/model_resolution_units.rs"]
tests/g_ai.rs:100:#[path = "suites/pi_config_trust.rs"]
tests/g_ai.rs:102:#[path = "suites/provider_config_units.rs"]
tests/g_ai.rs:104:#[path = "suites/quality_type_modeling.rs"]
tests/g_ai.rs:106:#[path = "suites/schema_catalog_w11.rs"]
tests/g_ai.rs:108:#[path = "suites/telemetry_core_w10.rs"]
tests/g_cli_server.rs:4:#[path = "suites/cli_config_resources.rs"]
tests/g_cli_server.rs:6:#[path = "suites/cli_entry_contract.rs"]
tests/g_cli_server.rs:8:#[path = "suites/cli_metadata_trust.rs"]
tests/g_cli_server.rs:10:#[path = "suites/cli_package_install.rs"]
tests/g_cli_server.rs:12:#[path = "suites/cli_parity_wave.rs"]
tests/g_cli_server.rs:14:#[path = "suites/client_reconnect.rs"]
tests/g_cli_server.rs:17:#[path = "suites/g_server_persistence.rs"]
tests/g_cli_server.rs:19:#[path = "suites/mcp_config_shape.rs"]
tests/g_cli_server.rs:21:#[path = "suites/powershell_tool.rs"]
tests/g_cli_server.rs:23:#[path = "suites/print_retry_json.rs"]
tests/g_cli_server.rs:25:#[path = "suites/rpc.rs"]
tests/g_cli_server.rs:27:#[path = "suites/rpc_commands.rs"]
tests/g_cli_server.rs:29:#[path = "suites/rpc_extension_ui.rs"]
tests/g_cli_server.rs:31:#[path = "suites/rpc_tool_selection.rs"]
tests/g_cli_server.rs:33:#[path = "suites/server_prompt_abort.rs"]
tests/g_cli_server.rs:35:#[path = "suites/server_tool_selection.rs"]
tests/g_cli_server.rs:37:#[path = "suites/server_unix.rs"]
tests/g_cli_server.rs:39:#[path = "suites/session_pick_units.rs"]
tests/g_cli_server.rs:41:#[path = "suites/session_resolve_units.rs"]
tests/g_cli_server.rs:43:#[path = "suites/settings_recovery.rs"]
tests/g_cli_server.rs:45:#[path = "suites/timing_gates.rs"]
tests/g_durable.rs:8:#[path = "suites/durable_assemble.rs"]
tests/g_durable.rs:10:#[path = "suites/durable_chord_diff.rs"]
tests/g_durable.rs:12:#[path = "suites/durable_context.rs"]
tests/g_durable.rs:14:#[path = "suites/durable_definitions.rs"]
tests/g_durable.rs:16:#[path = "suites/durable_delta.rs"]
tests/g_durable.rs:19:#[path = "suites/durable_attach.rs"]
tests/g_durable.rs:21:#[path = "suites/durable_facade.rs"]
tests/g_durable.rs:24:#[path = "suites/durable_harness_agent.rs"]
tests/g_durable.rs:26:#[path = "suites/durable_harness_settings.rs"]
tests/g_durable.rs:29:#[path = "suites/durable_inbox.rs"]
tests/g_durable.rs:32:#[path = "suites/durable_context_derive.rs"]
tests/g_durable.rs:34:#[path = "suites/durable_prompt.rs"]
tests/g_durable.rs:36:#[path = "suites/durable_scheduler.rs"]
tests/g_durable.rs:39:#[path = "suites/durable_builtin_documents.rs"]
tests/g_durable.rs:41:#[path = "suites/durable_publication.rs"]
tests/g_durable.rs:44:#[path = "suites/durable_compaction.rs"]
tests/g_durable.rs:46:#[path = "suites/durable_document_cache.rs"]
tests/g_durable.rs:48:#[path = "suites/durable_forks.rs"]
tests/g_durable.rs:50:#[path = "suites/durable_generation.rs"]
tests/g_durable.rs:52:#[path = "suites/durable_guard.rs"]
tests/g_durable.rs:54:#[path = "suites/durable_json.rs"]
tests/g_durable.rs:56:#[path = "suites/durable_lifecycle_w8.rs"]
tests/g_durable.rs:58:#[path = "suites/durable_live.rs"]
tests/g_durable.rs:60:#[path = "suites/durable_observation_units.rs"]
tests/g_durable.rs:62:#[path = "suites/durable_observer.rs"]
tests/g_durable.rs:64:#[path = "suites/durable_primitives.rs"]
tests/g_durable.rs:66:#[path = "suites/durable_registry.rs"]
tests/g_durable.rs:68:#[path = "suites/durable_sqlite_seam.rs"]
tests/g_durable.rs:70:#[path = "suites/durable_storage_conformance.rs"]
tests/g_durable.rs:72:#[path = "suites/durable_task_definition.rs"]
tests/g_durable.rs:74:#[path = "suites/durable_transaction.rs"]
tests/g_durable.rs:76:#[path = "suites/durable_usage.rs"]
tests/g_durable.rs:78:#[path = "suites/pi_format_export.rs"]
tests/g_durable.rs:80:#[path = "suites/pi_format_import.rs"]
tests/g_durable.rs:82:#[path = "suites/pi_format_roundtrip.rs"]
tests/g_durable.rs:85:#[path = "suites/durable_harness_documents.rs"]
tests/g_durable.rs:87:#[path = "suites/durable_submissions.rs"]
tests/g_durable.rs:89:#[path = "suites/durable_util.rs"]
tests/g_durable.rs:91:#[path = "suites/durable_view.rs"]
tests/g_durable.rs:94:#[path = "suites/durable_events.rs"]
tests/g_durable.rs:97:#[path = "suites/durable_harness.rs"]
tests/g_durable.rs:100:#[path = "suites/durable_tool.rs"]
tests/g_durable.rs:103:#[path = "suites/durable_task_graph.rs"]
tests/g_durable.rs:106:#[path = "suites/durable_trace_w8.rs"]
tests/g_durable.rs:109:#[path = "suites/durable_residuals_w11.rs"]
tests/g_extensions.rs:4:#[path = "suites/codemode_builtin.rs"]
tests/g_extensions.rs:6:#[path = "suites/codemode_strict.rs"]
tests/g_extensions.rs:8:#[path = "suites/codemode_worker.rs"]
tests/g_extensions.rs:10:#[path = "suites/extensions_audit_fixes.rs"]
tests/g_extensions.rs:12:#[path = "suites/extensions_events.rs"]
tests/g_extensions.rs:14:#[path = "suites/extensions_js_stream.rs"]
tests/g_extensions.rs:17:#[path = "suites/extensions_limits.rs"]
tests/g_extensions.rs:19:#[path = "suites/extensions_regressions.rs"]
tests/g_extensions.rs:22:#[path = "suites/extensions_stdio_routing.rs"]
tests/g_extensions.rs:24:#[path = "suites/extensions_wiring.rs"]
tests/g_extensions.rs:26:#[path = "suites/js_extension_api.rs"]
tests/g_extensions.rs:28:#[path = "suites/npm_resolve_units.rs"]
tests/g_extensions.rs:30:#[path = "suites/subagent_strict.rs"]
tests/g_extensions.rs:32:#[path = "suites/system_prompt_options_units.rs"]
tests/g_extensions.rs:34:#[path = "suites/user_bash_sink.rs"]
tests/g_print_tools_w14.rs:9:#[path = "suites/print_request_tools_w14.rs"]
tests/g_protocol.rs:4:#[path = "suites/cbor.rs"]
tests/g_protocol.rs:6:#[path = "suites/config_migrations.rs"]
tests/g_protocol.rs:8:#[path = "suites/config_schemas.rs"]
tests/g_protocol.rs:10:#[path = "suites/config_settings_units.rs"]
tests/g_protocol.rs:12:#[path = "suites/config_units.rs"]
tests/g_protocol.rs:14:#[path = "suites/forbidden_files.rs"]
tests/g_protocol.rs:16:#[path = "suites/framing.rs"]
tests/g_protocol.rs:18:#[path = "suites/lib_exports.rs"]
tests/g_protocol.rs:20:#[path = "suites/orchestration_convergence.rs"]
tests/g_protocol.rs:22:#[path = "suites/production_safety.rs"]
tests/g_protocol.rs:24:#[path = "suites/protocol.rs"]
tests/g_protocol.rs:26:#[path = "suites/schema_generator_w11.rs"]
tests/g_protocol.rs:28:#[path = "suites/source_hygiene.rs"]
tests/g_protocol.rs:30:#[path = "suites/upstream_examples.rs"]
tests/g_tools.rs:11:#[path = "suites/builtin_disable_w13.rs"]
tests/g_tools.rs:13:#[path = "suites/coding_agent_units.rs"]
tests/g_tools.rs:15:#[path = "suites/context_mode_exposure_w13.rs"]
tests/g_tools.rs:17:#[path = "suites/crash_log.rs"]
tests/g_tools.rs:19:#[path = "suites/export_html.rs"]
tests/g_tools.rs:21:#[path = "suites/g11_context_shadow.rs"]
tests/g_tools.rs:23:#[path = "suites/g11_skills_diagnostics.rs"]
tests/g_tools.rs:25:#[path = "suites/prompt_loadout_w22.rs"]
tests/g_tools.rs:28:#[path = "suites/python_vm_lock.rs"]
tests/g_tools.rs:30:#[path = "suites/read_image_tool.rs"]
tests/g_tools.rs:32:#[path = "suites/shell_tools.rs"]
tests/g_tools.rs:34:#[path = "suites/system_prompt.rs"]
tests/g_tools.rs:36:#[path = "suites/tool_exposure_parity_w14.rs"]
tests/g_tools.rs:38:#[path = "suites/tool_exposure_parity_w16.rs"]
tests/g_tools.rs:40:#[path = "suites/tool_exposure_virtual_models.rs"]
tests/g_tools.rs:42:#[path = "suites/tool_parity_h.rs"]
tests/g_tools.rs:44:#[path = "suites/tool_repair.rs"]
tests/g_tools.rs:46:#[path = "suites/tool_repair_grammar.rs"]
tests/g_tools.rs:48:#[path = "suites/tool_result_size_w17.rs"]
tests/g_tools.rs:50:#[path = "suites/tool_schema_validation.rs"]
tests/g_tools.rs:52:#[path = "suites/tool_schema_w11.rs"]
tests/g_tools.rs:54:#[path = "suites/tool_selection_parity_w13.rs"]
tests/g_tools.rs:56:#[path = "suites/tool_selection_units.rs"]
tests/g_tools.rs:58:#[path = "suites/tools_ab_round.rs"]
tests/g_tools.rs:60:#[path = "suites/tools_hardening.rs"]
tests/g_tools.rs:62:#[path = "suites/tools_security_hardening.rs"]
tests/g_tools.rs:64:#[path = "suites/tools_security_w9.rs"]
tests/g_tools.rs:66:#[path = "suites/tui_ansi_theme.rs"]
tests/g_tools.rs:68:#[path = "suites/write_persistence_nn.rs"]
tests/g_tui.rs:2:#[path = "suites/keybinding_actions.rs"]
tests/g_tui.rs:4:#[path = "suites/std_utils_w11.rs"]
tests/g_tui.rs:6:#[path = "suites/tui_alloc_trace_units.rs"]
tests/g_tui.rs:8:#[path = "suites/tui_cache_lane.rs"]
tests/g_tui.rs:10:#[path = "suites/tui_clipboard_paste.rs"]
tests/g_tui.rs:12:#[path = "suites/tui_core.rs"]
tests/g_tui.rs:14:#[path = "suites/tui_custom_overlay_w13.rs"]
tests/g_tui.rs:16:#[path = "suites/tui_editor.rs"]
tests/g_tui.rs:18:#[path = "suites/tui_editor_parity.rs"]
tests/g_tui.rs:20:#[path = "suites/tui_exit_surfaces.rs"]
tests/g_tui.rs:22:#[path = "suites/tui_extension_ui.rs"]
tests/g_tui.rs:24:#[path = "suites/tui_first_run_setup.rs"]
tests/g_tui.rs:26:#[path = "suites/tui_followup_parity.rs"]
tests/g_tui.rs:28:#[path = "suites/tui_gaps_w11.rs"]
tests/g_tui.rs:30:#[path = "suites/tui_grapheme_editor.rs"]
tests/g_tui.rs:32:#[path = "suites/tui_image_preview_units.rs"]
tests/g_tui.rs:34:#[path = "suites/tui_input_wakeup.rs"]
tests/g_tui.rs:36:#[path = "suites/tui_keybindings_upstream.rs"]
tests/g_tui.rs:38:#[path = "suites/tui_live_settings_w14.rs"]
tests/g_tui.rs:40:#[path = "suites/tui_markdown_autolinks_tables.rs"]
tests/g_tui.rs:42:#[path = "suites/tui_markdown_incremental.rs"]
tests/g_tui.rs:44:#[path = "suites/tui_markdown_parity.rs"]
tests/g_tui.rs:46:#[path = "suites/tui_messages_units.rs"]
tests/g_tui.rs:48:#[path = "suites/tui_output_writer_w10.rs"]
tests/g_tui.rs:50:#[path = "suites/tui_overlay_float_w14.rs"]
tests/g_tui.rs:52:#[path = "suites/tui_parity_round2.rs"]
tests/g_tui.rs:54:#[path = "suites/tui_parity_wave.rs"]
tests/g_tui.rs:56:#[path = "suites/tui_program_status.rs"]
tests/g_tui.rs:58:#[path = "suites/tui_r_altscreen.rs"]
tests/g_tui.rs:60:#[path = "suites/tui_read_header_w14.rs"]
tests/g_tui.rs:62:#[path = "suites/tui_refresh.rs"]
tests/g_tui.rs:64:#[path = "suites/tui_regressions.rs"]
tests/g_tui.rs:66:#[path = "suites/tui_render_hotpath_w10.rs"]
tests/g_tui.rs:68:#[path = "suites/tui_render_residue.rs"]
tests/g_tui.rs:70:#[path = "suites/tui_renderer_w9.rs"]
tests/g_tui.rs:72:#[path = "suites/tui_session_cwd.rs"]
tests/g_tui.rs:74:#[path = "suites/tui_session_manage.rs"]
tests/g_tui.rs:76:#[path = "suites/tui_sessions.rs"]
tests/g_tui.rs:78:#[path = "suites/tui_settings_diagnostics.rs"]
tests/g_tui.rs:80:#[path = "suites/tui_slash_commands.rs"]
tests/g_tui.rs:82:#[path = "suites/tui_stdin_decoding.rs"]
tests/g_tui.rs:84:#[path = "suites/tui_surface_units.rs"]
tests/g_tui.rs:86:#[path = "suites/tui_syntax_highlight.rs"]
tests/g_tui.rs:88:#[path = "suites/tui_system_theme.rs"]
tests/g_tui.rs:90:#[path = "suites/tui_theme_colors.rs"]
tests/g_tui.rs:92:#[path = "suites/tui_theme_g9.rs"]
tests/g_tui.rs:94:#[path = "suites/tui_tool_display.rs"]
tests/g_tui.rs:96:#[path = "suites/tui_tool_images_w14.rs"]
tests/g_tui.rs:98:#[path = "suites/tui_units.rs"]
tests/g_tui.rs:100:#[path = "suites/tui_widget_defer_w10.rs"]
tests/tui_snapshot_refresh_w16.rs:8:#[path = "suites/tui_snapshot_refresh_w16.rs"]
```

### 3. `[[test]]` targets declared in `Cargo.toml`

```console
$ grep -n -A2 '^\[\[test\]\]' Cargo.toml
948:[[test]]
949-name = "zz_merged_1"
950-path = "tests/zz_merged_1.rs"
--
952:[[test]]
953-name = "zz_merged_2"
954-path = "tests/zz_merged_2.rs"
--
956:[[test]]
957-name = "zz_merged_3"
958-path = "tests/zz_merged_3.rs"
--
960:[[test]]
961-name = "zz_merged_4"
962-path = "tests/zz_merged_4.rs"
--
964:[[test]]
965-name = "zz_merged_5"
966-path = "tests/zz_merged_5.rs"
--
968:[[test]]
969-name = "zz_merged_6"
970-path = "tests/zz_merged_6.rs"
--
972:[[test]]
973-name = "zz_merged_7"
974-path = "tests/zz_merged_7.rs"
--
976:[[test]]
977-name = "zz_merged_8"
978-path = "tests/zz_merged_8.rs"
--
980:[[test]]
981-name = "python_session_lifecycle"
982-path = "tests/python_session_lifecycle.rs"
--
984:[[test]]
985-name = "worker_threads_real"
986-path = "tests/worker_threads_real.rs"
--
988:[[test]]
989-name = "durable_memcopy_bench"
990-path = "tests/durable_memcopy_bench.rs"
--
992:[[test]]
993-name = "g_durable"
994-path = "tests/g_durable.rs"
--
996:[[test]]
997-name = "print_native_session"
998-path = "tests/print_native_session.rs"
```

### 4. How the verification was done

- The 332 `#[path = ...]` lines in grep (1) were parsed; for each, the
  included file and its `mod` name were recorded and the include graph was walked
  transitively from the 8 `zz_merged_*` targets and the 5 standalone targets.
- The 294 `#[path = "suites/..."]` lines in grep (2) were parsed the same way;
  each suite has exactly one wrapper, so each suite resolves to exactly one
  harness target (no duplicates, no orphans).
- grep (3) supplies the standalone target names (`g_durable`,
  `python_session_lifecycle`, `worker_threads_real`, `durable_memcopy_bench`,
  `print_native_session`) that are not reachable from a `zz_merged_*` file.
- Result: 294 suites files + 332 root files reachable from `zz_merged_*` +
  5 standalone root files = 631 mapped rows; 6 `#[global_allocator]` originals
  are deliberately not targets. (Re-derived 2026-11-14: all 631 rows and the
  three greps above still match the tree; the `include!` fragment described
  above carries no `#[path]` and is outside these counts.)

---

One-line summary: 631 of the 637 `tests/suites/*.rs` + `tests/*.rs` files are compiled by 13 explicit `[[test]]` targets (the 8 `zz_merged_*` harnesses plus `g_durable`/`python_session_lifecycle`/`worker_threads_real`/`durable_memcopy_bench`/`print_native_session`), and `scripts/which_harness.sh <name|path>` prints the harness and the ready `cargo nextest run -p pir-sdk --test ... -E 'test(<module>::)'` command.
