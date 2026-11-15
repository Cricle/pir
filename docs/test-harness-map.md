# Test Harness Map

> Re-derived 2026-11-15 (PIR-QE-002 option 1) after regenerating the harnesses
> with `scripts/gen_merged_tests.ps1`; every row comes from the real `#[path]`
> lines in the on-disk `tests/zz_merged_*` harnesses and the `[[test]]` block,
> and `gen_merged_tests.ps1 -Check` is green at this revision.

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
| `zz_merged_1` | `tests/zz_merged_1.rs` | merged harness | 66 |
| `zz_merged_2` | `tests/zz_merged_2.rs` | merged harness | 65 |
| `zz_merged_3` | `tests/zz_merged_3.rs` | merged harness | 64 |
| `zz_merged_4` | `tests/zz_merged_4.rs` | merged harness | 64 |
| `zz_merged_5` | `tests/zz_merged_5.rs` | merged harness | 52 |
| `zz_merged_6` | `tests/zz_merged_6.rs` | merged harness | 2 |
| `zz_merged_7` | `tests/zz_merged_7.rs` | merged harness | 2 |
| `zz_merged_8` | `tests/zz_merged_8.rs` | merged harness | 17 |
| `python_session_lifecycle` | `tests/python_session_lifecycle.rs` | standalone | 0 |
| `worker_threads_real` | `tests/worker_threads_real.rs` | standalone | 0 |
| `durable_memcopy_bench` | `tests/durable_memcopy_bench.rs` | standalone | 0 |
| `g_durable` | `tests/g_durable.rs` | standalone | 45 |
| `print_native_session` | `tests/print_native_session.rs` | standalone | 0 |

## Mapping — `tests/suites/*.rs` (295 files)

`wrapper` is the `tests/*.rs` file that includes the suite with
`#[path = "suites/....rs"]`; the wrapper is then included by the harness (or is a
standalone `[[test]]` target). The filter column is the exact nextest `test(...)`
predicate for that suite inside that harness.

| # | suites file | wrapper | harness target | nextest filter |
|---:|---|---|---|---|
| 1 | `tests/suites/agent_bootstrap.rs` | `tests/agent_bootstrap.rs` | `zz_merged_5` | `-E 'test(agent_bootstrap::agent_bootstrap::)'` |
| 2 | `tests/suites/agent_core_w11.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_core_w11::)'` |
| 3 | `tests/suites/agent_core_w8.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_core_w8::)'` |
| 4 | `tests/suites/agent_dependencies.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_dependencies::)'` |
| 5 | `tests/suites/agent_dir_identity_w16.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_dir_identity_w16::)'` |
| 6 | `tests/suites/agent_loop.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_loop::)'` |
| 7 | `tests/suites/agent_loop_parity.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_loop_parity::)'` |
| 8 | `tests/suites/agent_loop_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_loop_units::)'` |
| 9 | `tests/suites/agent_session.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_session::)'` |
| 10 | `tests/suites/agent_session_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_session_units::)'` |
| 11 | `tests/suites/agent_tool_execution.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_tool_execution::)'` |
| 12 | `tests/suites/agent_tool_types.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_tool_types::)'` |
| 13 | `tests/suites/agent_tool_workflow.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_tool_workflow::)'` |
| 14 | `tests/suites/agent_tools.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::agent_tools::)'` |
| 15 | `tests/suites/ai_abort_partial_w13.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_abort_partial_w13::)'` |
| 16 | `tests/suites/ai_abort_signal.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_abort_signal::)'` |
| 17 | `tests/suites/ai_adapter_fixes.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_adapter_fixes::)'` |
| 18 | `tests/suites/ai_adapter_parity.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_adapter_parity::)'` |
| 19 | `tests/suites/ai_adapters_parity.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_adapters_parity::)'` |
| 20 | `tests/suites/ai_adapters_w11.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_adapters_w11::)'` |
| 21 | `tests/suites/ai_auth.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_auth::)'` |
| 22 | `tests/suites/ai_bedrock_credentials.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_bedrock_credentials::)'` |
| 23 | `tests/suites/ai_catalog_full_w10.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_catalog_full_w10::)'` |
| 24 | `tests/suites/ai_catalog_w10.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_catalog_w10::)'` |
| 25 | `tests/suites/ai_codex_ws_w10.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_codex_ws_w10::)'` |
| 26 | `tests/suites/ai_codex_ws_w13.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_codex_ws_w13::)'` |
| 27 | `tests/suites/ai_copilot_headers.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_copilot_headers::)'` |
| 28 | `tests/suites/ai_error.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_error::)'` |
| 29 | `tests/suites/ai_event_stream.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_event_stream::)'` |
| 30 | `tests/suites/ai_event_stream_backpressure.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_event_stream_backpressure::)'` |
| 31 | `tests/suites/ai_event_stream_drop_w13.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_event_stream_drop_w13::)'` |
| 32 | `tests/suites/ai_event_stream_ext.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_event_stream_ext::)'` |
| 33 | `tests/suites/ai_google.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_google::)'` |
| 34 | `tests/suites/ai_http.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_http::)'` |
| 35 | `tests/suites/ai_malformed_args_w10.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_malformed_args_w10::)'` |
| 36 | `tests/suites/ai_pi_messages_cow.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_pi_messages_cow::)'` |
| 37 | `tests/suites/ai_producer_liveness_w13.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_producer_liveness_w13::)'` |
| 38 | `tests/suites/ai_provider.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_provider::)'` |
| 39 | `tests/suites/ai_provider_contracts.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_provider_contracts::)'` |
| 40 | `tests/suites/ai_remaining_adapter_contracts.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_remaining_adapter_contracts::)'` |
| 41 | `tests/suites/ai_responses_tool_changes.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_responses_tool_changes::)'` |
| 42 | `tests/suites/ai_retry_overflow_d2.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_retry_overflow_d2::)'` |
| 43 | `tests/suites/ai_sse.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_sse::)'` |
| 44 | `tests/suites/ai_sse_proptest.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_sse_proptest::)'` |
| 45 | `tests/suites/ai_telemetry_w11.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_telemetry_w11::)'` |
| 46 | `tests/suites/ai_transcript.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_transcript::)'` |
| 47 | `tests/suites/ai_transport_idle.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_transport_idle::)'` |
| 48 | `tests/suites/ai_types.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_types::)'` |
| 49 | `tests/suites/ai_util.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::ai_util::)'` |
| 50 | `tests/suites/auth_cli.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::auth_cli::)'` |
| 51 | `tests/suites/auth_oauth.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::auth_oauth::)'` |
| 52 | `tests/suites/auth_oauth_unit.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::auth_oauth_unit::)'` |
| 53 | `tests/suites/background_tool_calls.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::background_tool_calls::)'` |
| 54 | `tests/suites/branch_summarization.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::branch_summarization::)'` |
| 55 | `tests/suites/builtin_disable_w13.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::builtin_disable_w13::)'` |
| 56 | `tests/suites/builtins_cross.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::builtins_cross::)'` |
| 57 | `tests/suites/cache_warmer.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::cache_warmer::)'` |
| 58 | `tests/suites/catalog_drift.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::catalog_drift::)'` |
| 59 | `tests/suites/catalog_units.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::catalog_units::)'` |
| 60 | `tests/suites/cbor.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::cbor::)'` |
| 61 | `tests/suites/cli_config_resources.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::cli_config_resources::)'` |
| 62 | `tests/suites/cli_entry_contract.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::cli_entry_contract::)'` |
| 63 | `tests/suites/cli_metadata_trust.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::cli_metadata_trust::)'` |
| 64 | `tests/suites/cli_package_install.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::cli_package_install::)'` |
| 65 | `tests/suites/cli_parity_wave.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::cli_parity_wave::)'` |
| 66 | `tests/suites/client_reconnect.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::client_reconnect::)'` |
| 67 | `tests/suites/codemode_builtin.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::codemode_builtin::)'` |
| 68 | `tests/suites/codemode_strict.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::codemode_strict::)'` |
| 69 | `tests/suites/codemode_worker.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::codemode_worker::)'` |
| 70 | `tests/suites/coding_agent_units.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::coding_agent_units::)'` |
| 71 | `tests/suites/compaction.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::compaction::)'` |
| 72 | `tests/suites/compaction_chunking.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::compaction_chunking::)'` |
| 73 | `tests/suites/config_cache_lifetime_w16.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::config_cache_lifetime_w16::)'` |
| 74 | `tests/suites/config_migrations.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::config_migrations::)'` |
| 75 | `tests/suites/config_schemas.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::config_schemas::)'` |
| 76 | `tests/suites/config_settings_units.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::config_settings_units::)'` |
| 77 | `tests/suites/config_units.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::config_units::)'` |
| 78 | `tests/suites/context_mode_exposure_w13.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::context_mode_exposure_w13::)'` |
| 79 | `tests/suites/context_usage_projection_estimate.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::context_usage_projection_estimate::)'` |
| 80 | `tests/suites/crash_log.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::crash_log::)'` |
| 81 | `tests/suites/durable_assemble.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_assemble::)'` |
| 82 | `tests/suites/durable_attach.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_attach::)'` |
| 83 | `tests/suites/durable_builtin_documents.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_builtin_documents::)'` |
| 84 | `tests/suites/durable_chord_diff.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_chord_diff::)'` |
| 85 | `tests/suites/durable_compaction.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_compaction::)'` |
| 86 | `tests/suites/durable_context.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_context::)'` |
| 87 | `tests/suites/durable_context_derive.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_context_derive::)'` |
| 88 | `tests/suites/durable_definitions.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_definitions::)'` |
| 89 | `tests/suites/durable_delta.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_delta::)'` |
| 90 | `tests/suites/durable_document_cache.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_document_cache::)'` |
| 91 | `tests/suites/durable_events.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_events::)'` |
| 92 | `tests/suites/durable_facade.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_facade::)'` |
| 93 | `tests/suites/durable_forks.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_forks::)'` |
| 94 | `tests/suites/durable_generation.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_generation::)'` |
| 95 | `tests/suites/durable_guard.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_guard::)'` |
| 96 | `tests/suites/durable_harness.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_harness::)'` |
| 97 | `tests/suites/durable_harness_agent.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_harness_agent::)'` |
| 98 | `tests/suites/durable_harness_documents.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_harness_documents::)'` |
| 99 | `tests/suites/durable_harness_settings.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_harness_settings::)'` |
| 100 | `tests/suites/durable_inbox.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_inbox::)'` |
| 101 | `tests/suites/durable_json.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_json::)'` |
| 102 | `tests/suites/durable_lifecycle_w8.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_lifecycle_w8::)'` |
| 103 | `tests/suites/durable_live.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_live::)'` |
| 104 | `tests/suites/durable_observation_units.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_observation_units::)'` |
| 105 | `tests/suites/durable_observer.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_observer::)'` |
| 106 | `tests/suites/durable_primitives.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_primitives::)'` |
| 107 | `tests/suites/durable_prompt.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_prompt::)'` |
| 108 | `tests/suites/durable_publication.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_publication::)'` |
| 109 | `tests/suites/durable_registry.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_registry::)'` |
| 110 | `tests/suites/durable_residuals_w11.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_residuals_w11::)'` |
| 111 | `tests/suites/durable_scheduler.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_scheduler::)'` |
| 112 | `tests/suites/durable_sqlite_seam.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_sqlite_seam::)'` |
| 113 | `tests/suites/durable_storage_conformance.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_storage_conformance::)'` |
| 114 | `tests/suites/durable_submissions.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_submissions::)'` |
| 115 | `tests/suites/durable_task_definition.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_task_definition::)'` |
| 116 | `tests/suites/durable_task_graph.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_task_graph::)'` |
| 117 | `tests/suites/durable_tool.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_tool::)'` |
| 118 | `tests/suites/durable_trace_w8.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_trace_w8::)'` |
| 119 | `tests/suites/durable_transaction.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_transaction::)'` |
| 120 | `tests/suites/durable_usage.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_usage::)'` |
| 121 | `tests/suites/durable_util.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_util::)'` |
| 122 | `tests/suites/durable_view.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(durable_view::)'` |
| 123 | `tests/suites/env_identity_w18.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::env_identity_w18::)'` |
| 124 | `tests/suites/export_html.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::export_html::)'` |
| 125 | `tests/suites/extensions_audit_fixes.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_audit_fixes::)'` |
| 126 | `tests/suites/extensions_events.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_events::)'` |
| 127 | `tests/suites/extensions_js_stream.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_js_stream::)'` |
| 128 | `tests/suites/extensions_limits.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_limits::)'` |
| 129 | `tests/suites/extensions_regressions.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_regressions::)'` |
| 130 | `tests/suites/extensions_stdio_routing.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_stdio_routing::)'` |
| 131 | `tests/suites/extensions_wiring.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::extensions_wiring::)'` |
| 132 | `tests/suites/forbidden_files.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::forbidden_files::)'` |
| 133 | `tests/suites/framing.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::framing::)'` |
| 134 | `tests/suites/g11_context_shadow.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::g11_context_shadow::)'` |
| 135 | `tests/suites/g11_provider_attribution.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::g11_provider_attribution::)'` |
| 136 | `tests/suites/g11_skills_diagnostics.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::g11_skills_diagnostics::)'` |
| 137 | `tests/suites/g_server_persistence.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::g_server_persistence::)'` |
| 138 | `tests/suites/globset_w11.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::globset_w11::)'` |
| 139 | `tests/suites/hooks_payload_w20.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::hooks_payload_w20::)'` |
| 140 | `tests/suites/hooks_session_w21.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::hooks_session_w21::)'` |
| 141 | `tests/suites/js_extension_api.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::js_extension_api::)'` |
| 142 | `tests/suites/keybinding_actions.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::keybinding_actions::)'` |
| 143 | `tests/suites/lib_exports.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::lib_exports::)'` |
| 144 | `tests/suites/mcp_agent_dir_w16.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_agent_dir_w16::)'` |
| 145 | `tests/suites/mcp_config_shape.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::mcp_config_shape::)'` |
| 146 | `tests/suites/mcp_env_w11.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_env_w11::)'` |
| 147 | `tests/suites/mcp_plugin_root_w17.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_plugin_root_w17::)'` |
| 148 | `tests/suites/mcp_prune_w18.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_prune_w18::)'` |
| 149 | `tests/suites/mcp_residuals_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_residuals_w13::)'` |
| 150 | `tests/suites/mcp_signin_text_w16.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_signin_text_w16::)'` |
| 151 | `tests/suites/mcp_slash_command_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_slash_command_w13::)'` |
| 152 | `tests/suites/mcp_status_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_status_w13::)'` |
| 153 | `tests/suites/mcp_toggle_refresh_w18.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::mcp_toggle_refresh_w18::)'` |
| 154 | `tests/suites/memory_builtin.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::memory_builtin::)'` |
| 155 | `tests/suites/model_catalog.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::model_catalog::)'` |
| 156 | `tests/suites/model_catalog_strict.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::model_catalog_strict::)'` |
| 157 | `tests/suites/model_resolution_units.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::model_resolution_units::)'` |
| 158 | `tests/suites/model_switch_auth_w17.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::model_switch_auth_w17::)'` |
| 159 | `tests/suites/native_host_w19.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::native_host_w19::)'` |
| 160 | `tests/suites/npm_resolve_units.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::npm_resolve_units::)'` |
| 161 | `tests/suites/orchestration_convergence.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::orchestration_convergence::)'` |
| 162 | `tests/suites/parallel_concurrency.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::parallel_concurrency::)'` |
| 163 | `tests/suites/phone_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::phone_units::)'` |
| 164 | `tests/suites/pi_config_trust.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::pi_config_trust::)'` |
| 165 | `tests/suites/pi_format_export.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(pi_format_export::)'` |
| 166 | `tests/suites/pi_format_import.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(pi_format_import::)'` |
| 167 | `tests/suites/pi_format_roundtrip.rs` | `tests/g_durable.rs` | `g_durable` | `-E 'test(pi_format_roundtrip::)'` |
| 168 | `tests/suites/plugin_events_w16.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugin_events_w16::)'` |
| 169 | `tests/suites/plugin_ledger_w14.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugin_ledger_w14::)'` |
| 170 | `tests/suites/plugin_services_w10.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugin_services_w10::)'` |
| 171 | `tests/suites/plugins_registry_gate.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugins_registry_gate::)'` |
| 172 | `tests/suites/plugins_unified.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugins_unified::)'` |
| 173 | `tests/suites/plugins_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugins_units::)'` |
| 174 | `tests/suites/plugins_w11.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::plugins_w11::)'` |
| 175 | `tests/suites/powershell_tool.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::powershell_tool::)'` |
| 176 | `tests/suites/print_callability_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::print_callability_w13::)'` |
| 177 | `tests/suites/print_mode.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::print_mode::)'` |
| 178 | `tests/suites/print_request_tools_w14.rs` | `tests/g_print_tools_w14.rs` | `zz_merged_1` | `-E 'test(g_print_tools_w14::print_request_tools_w14::)'` |
| 179 | `tests/suites/print_retry_json.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::print_retry_json::)'` |
| 180 | `tests/suites/production_safety.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::production_safety::)'` |
| 181 | `tests/suites/prompt_loadout_w22.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::prompt_loadout_w22::)'` |
| 182 | `tests/suites/protocol.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::protocol::)'` |
| 183 | `tests/suites/provider_config_units.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::provider_config_units::)'` |
| 184 | `tests/suites/python_vm_lock.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::python_vm_lock::)'` |
| 185 | `tests/suites/quality_type_modeling.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::quality_type_modeling::)'` |
| 186 | `tests/suites/read_image_tool.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::read_image_tool::)'` |
| 187 | `tests/suites/reload_native_w14.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::reload_native_w14::)'` |
| 188 | `tests/suites/resume_system_prompt_once.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::resume_system_prompt_once::)'` |
| 189 | `tests/suites/rpc.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::rpc::)'` |
| 190 | `tests/suites/rpc_commands.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::rpc_commands::)'` |
| 191 | `tests/suites/rpc_extension_ui.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::rpc_extension_ui::)'` |
| 192 | `tests/suites/rpc_tool_selection.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::rpc_tool_selection::)'` |
| 193 | `tests/suites/run_diagnostics.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::run_diagnostics::)'` |
| 194 | `tests/suites/schema_catalog_w11.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::schema_catalog_w11::)'` |
| 195 | `tests/suites/schema_generator_w11.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::schema_generator_w11::)'` |
| 196 | `tests/suites/sdk.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk::)'` |
| 197 | `tests/suites/sdk_agent_isolation_w16.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_agent_isolation_w16::)'` |
| 198 | `tests/suites/sdk_defaults_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_defaults_w13::)'` |
| 199 | `tests/suites/sdk_event_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_event_units::)'` |
| 200 | `tests/suites/sdk_gaps_w11.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_gaps_w11::)'` |
| 201 | `tests/suites/sdk_plugins_w8.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_plugins_w8::)'` |
| 202 | `tests/suites/sdk_proxy_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_proxy_w13::)'` |
| 203 | `tests/suites/sdk_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::sdk_units::)'` |
| 204 | `tests/suites/server_prompt_abort.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::server_prompt_abort::)'` |
| 205 | `tests/suites/server_tool_selection.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::server_tool_selection::)'` |
| 206 | `tests/suites/server_unix.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::server_unix::)'` |
| 207 | `tests/suites/session_commands.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_commands::)'` |
| 208 | `tests/suites/session_compaction_w11.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_compaction_w11::)'` |
| 209 | `tests/suites/session_ctx_units.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_ctx_units::)'` |
| 210 | `tests/suites/session_events_parity.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_events_parity::)'` |
| 211 | `tests/suites/session_file_contract_w14.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_file_contract_w14::)'` |
| 212 | `tests/suites/session_manager.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_manager::)'` |
| 213 | `tests/suites/session_pick_units.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::session_pick_units::)'` |
| 214 | `tests/suites/session_resolve_units.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::session_resolve_units::)'` |
| 215 | `tests/suites/session_rewrite_w13.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::session_rewrite_w13::)'` |
| 216 | `tests/suites/settings_recovery.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::settings_recovery::)'` |
| 217 | `tests/suites/shell_tools.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::shell_tools::)'` |
| 218 | `tests/suites/source_hygiene.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::source_hygiene::)'` |
| 219 | `tests/suites/startup_thinking_clamp.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::startup_thinking_clamp::)'` |
| 220 | `tests/suites/std_utils_w11.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::std_utils_w11::)'` |
| 221 | `tests/suites/subagent_strict.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::subagent_strict::)'` |
| 222 | `tests/suites/system_prompt.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::system_prompt::)'` |
| 223 | `tests/suites/system_prompt_options_units.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::system_prompt_options_units::)'` |
| 224 | `tests/suites/telemetry_core_w10.rs` | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::telemetry_core_w10::)'` |
| 225 | `tests/suites/timing_gates.rs` | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::timing_gates::)'` |
| 226 | `tests/suites/todo_builtin.rs` | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::todo_builtin::)'` |
| 227 | `tests/suites/tool_exposure_parity_w14.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_exposure_parity_w14::)'` |
| 228 | `tests/suites/tool_exposure_parity_w16.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_exposure_parity_w16::)'` |
| 229 | `tests/suites/tool_exposure_virtual_models.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_exposure_virtual_models::)'` |
| 230 | `tests/suites/tool_parity_h.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_parity_h::)'` |
| 231 | `tests/suites/tool_repair.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_repair::)'` |
| 232 | `tests/suites/tool_repair_grammar.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_repair_grammar::)'` |
| 233 | `tests/suites/tool_result_size_w17.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_result_size_w17::)'` |
| 234 | `tests/suites/tool_schema_validation.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_schema_validation::)'` |
| 235 | `tests/suites/tool_schema_w11.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_schema_w11::)'` |
| 236 | `tests/suites/tool_selection_parity_w13.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_selection_parity_w13::)'` |
| 237 | `tests/suites/tool_selection_units.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tool_selection_units::)'` |
| 238 | `tests/suites/tools_ab_round.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tools_ab_round::)'` |
| 239 | `tests/suites/tools_hardening.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tools_hardening::)'` |
| 240 | `tests/suites/tools_security_hardening.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tools_security_hardening::)'` |
| 241 | `tests/suites/tools_security_w9.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tools_security_w9::)'` |
| 242 | `tests/suites/tui_alloc_trace_units.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_alloc_trace_units::)'` |
| 243 | `tests/suites/tui_ansi_theme.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::tui_ansi_theme::)'` |
| 244 | `tests/suites/tui_burst_latency_w22.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_burst_latency_w22::)'` |
| 245 | `tests/suites/tui_cache_lane.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_cache_lane::)'` |
| 246 | `tests/suites/tui_clipboard_paste.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_clipboard_paste::)'` |
| 247 | `tests/suites/tui_core.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_core::)'` |
| 248 | `tests/suites/tui_custom_overlay_w13.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_custom_overlay_w13::)'` |
| 249 | `tests/suites/tui_editor.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_editor::)'` |
| 250 | `tests/suites/tui_editor_parity.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_editor_parity::)'` |
| 251 | `tests/suites/tui_exit_surfaces.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_exit_surfaces::)'` |
| 252 | `tests/suites/tui_extension_ui.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_extension_ui::)'` |
| 253 | `tests/suites/tui_first_run_setup.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_first_run_setup::)'` |
| 254 | `tests/suites/tui_followup_parity.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_followup_parity::)'` |
| 255 | `tests/suites/tui_gaps_w11.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_gaps_w11::)'` |
| 256 | `tests/suites/tui_grapheme_editor.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_grapheme_editor::)'` |
| 257 | `tests/suites/tui_image_preview_units.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_image_preview_units::)'` |
| 258 | `tests/suites/tui_input_wakeup.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_input_wakeup::)'` |
| 259 | `tests/suites/tui_keybindings_upstream.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_keybindings_upstream::)'` |
| 260 | `tests/suites/tui_live_settings_w14.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_live_settings_w14::)'` |
| 261 | `tests/suites/tui_markdown_autolinks_tables.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_markdown_autolinks_tables::)'` |
| 262 | `tests/suites/tui_markdown_incremental.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_markdown_incremental::)'` |
| 263 | `tests/suites/tui_markdown_parity.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_markdown_parity::)'` |
| 264 | `tests/suites/tui_messages_units.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_messages_units::)'` |
| 265 | `tests/suites/tui_output_writer_w10.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_output_writer_w10::)'` |
| 266 | `tests/suites/tui_overlay_float_w14.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_overlay_float_w14::)'` |
| 267 | `tests/suites/tui_parity_round2.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_parity_round2::)'` |
| 268 | `tests/suites/tui_parity_wave.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_parity_wave::)'` |
| 269 | `tests/suites/tui_program_status.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_program_status::)'` |
| 270 | `tests/suites/tui_r_altscreen.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_r_altscreen::)'` |
| 271 | `tests/suites/tui_read_header_w14.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_read_header_w14::)'` |
| 272 | `tests/suites/tui_refresh.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_refresh::)'` |
| 273 | `tests/suites/tui_regressions.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_regressions::)'` |
| 274 | `tests/suites/tui_render_hotpath_w10.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_render_hotpath_w10::)'` |
| 275 | `tests/suites/tui_render_residue.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_render_residue::)'` |
| 276 | `tests/suites/tui_renderer_w9.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_renderer_w9::)'` |
| 277 | `tests/suites/tui_session_cwd.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_session_cwd::)'` |
| 278 | `tests/suites/tui_session_manage.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_session_manage::)'` |
| 279 | `tests/suites/tui_sessions.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_sessions::)'` |
| 280 | `tests/suites/tui_settings_diagnostics.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_settings_diagnostics::)'` |
| 281 | `tests/suites/tui_slash_commands.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_slash_commands::)'` |
| 282 | `tests/suites/tui_snapshot_refresh_w16.rs` | `tests/tui_snapshot_refresh_w16.rs` | `zz_merged_1` | `-E 'test(tui_snapshot_refresh_w16::tui_snapshot_refresh_w16::)'` |
| 283 | `tests/suites/tui_stdin_decoding.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_stdin_decoding::)'` |
| 284 | `tests/suites/tui_surface_units.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_surface_units::)'` |
| 285 | `tests/suites/tui_syntax_highlight.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_syntax_highlight::)'` |
| 286 | `tests/suites/tui_system_theme.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_system_theme::)'` |
| 287 | `tests/suites/tui_theme_colors.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_theme_colors::)'` |
| 288 | `tests/suites/tui_theme_g9.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_theme_g9::)'` |
| 289 | `tests/suites/tui_tool_display.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_tool_display::)'` |
| 290 | `tests/suites/tui_tool_images_w14.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_tool_images_w14::)'` |
| 291 | `tests/suites/tui_units.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_units::)'` |
| 292 | `tests/suites/tui_widget_defer_w10.rs` | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::tui_widget_defer_w10::)'` |
| 293 | `tests/suites/upstream_examples.rs` | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::upstream_examples::)'` |
| 294 | `tests/suites/user_bash_sink.rs` | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::user_bash_sink::)'` |
| 295 | `tests/suites/write_persistence_nn.rs` | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::write_persistence_nn::)'` |

## Mapping — `tests/*.rs` root test files (331 compiled + 0 uncompiled)

Root files that are themselves named wrapper/harness helpers (e.g. `g_agent.rs`)
are compiled by the `zz_merged_*` harness that includes them; the filter runs their
own tests (and, for a wrapper, its `suites/` submodules as well).

| # | root test file | harness target | nextest filter |
|---:|---|---|---|
| 1 | `tests/abort_wakeup.rs` | `zz_merged_4` | `-E 'test(abort_wakeup::)'` |
| 2 | `tests/agent_benchmark.rs` | `zz_merged_8` | `-E 'test(agent_benchmark::)'` |
| 3 | `tests/agent_bootstrap.rs` | `zz_merged_5` | `-E 'test(agent_bootstrap::)'` |
| 4 | `tests/agent_end_shares_result.rs` | `zz_merged_4` | `-E 'test(agent_end_shares_result::)'` |
| 5 | `tests/agent_mutation_serialization_fix.rs` | `zz_merged_5` | `-E 'test(agent_mutation_serialization_fix::)'` |
| 6 | `tests/agent_scheduler_hot_path.rs` | `zz_merged_4` | `-E 'test(agent_scheduler_hot_path::)'` |
| 7 | `tests/agents_skills_discovery.rs` | `zz_merged_5` | `-E 'test(agents_skills_discovery::)'` |
| 8 | `tests/ai_bedrock_credentials.rs` | `zz_merged_1` | `-E 'test(ai_bedrock_credentials::)'` |
| 9 | `tests/ai_lane_g_w17.rs` | `zz_merged_3` | `-E 'test(ai_lane_g_w17::)'` |
| 10 | `tests/ai_payload_body_reuse.rs` | `zz_merged_3` | `-E 'test(ai_payload_body_reuse::)'` |
| 11 | `tests/ai_proxy.rs` | `zz_merged_1` | `-E 'test(ai_proxy::)'` |
| 12 | `tests/ai_request_snapshots.rs` | `zz_merged_4` | `-E 'test(ai_request_snapshots::)'` |
| 13 | `tests/ai_shims.rs` | `zz_merged_2` | `-E 'test(ai_shims::)'` |
| 14 | `tests/ai_transport_seam.rs` | `zz_merged_2` | `-E 'test(ai_transport_seam::)'` |
| 15 | `tests/altscreen_parity.rs` | `zz_merged_1` | `-E 'test(altscreen_parity::)'` |
| 16 | `tests/assert_shim.rs` | `zz_merged_5` | `-E 'test(assert_shim::)'` |
| 17 | `tests/async_hooks_shims.rs` | `zz_merged_5` | `-E 'test(async_hooks_shims::)'` |
| 18 | `tests/auth_storage_async.rs` | `zz_merged_2` | `-E 'test(auth_storage_async::)'` |
| 19 | `tests/backend_copy_audit.rs` | `zz_merged_3` | `-E 'test(backend_copy_audit::)'` |
| 20 | `tests/background_model_tools.rs` | `zz_merged_3` | `-E 'test(background_model_tools::)'` |
| 21 | `tests/bash_abort_set.rs` | `zz_merged_1` | `-E 'test(bash_abort_set::)'` |
| 22 | `tests/bash_eviction_utf8.rs` | `zz_merged_3` | `-E 'test(bash_eviction_utf8::)'` |
| 23 | `tests/bash_full_output_bounded.rs` | `zz_merged_7` | `-E 'test(bash_full_output_bounded::)'` |
| 24 | `tests/bash_tree_kill_prompt.rs` | `zz_merged_5` | `-E 'test(bash_tree_kill_prompt::)'` |
| 25 | `tests/bcrypt_shims.rs` | `zz_merged_4` | `-E 'test(bcrypt_shims::)'` |
| 26 | `tests/bedrock_chain_extensions.rs` | `zz_merged_1` | `-E 'test(bedrock_chain_extensions::)'` |
| 27 | `tests/bedrock_p1_fixes.rs` | `zz_merged_3` | `-E 'test(bedrock_p1_fixes::)'` |
| 28 | `tests/brush_embed.rs` | `zz_merged_1` | `-E 'test(brush_embed::)'` |
| 29 | `tests/brush_embed_gate.rs` | `zz_merged_3` | `-E 'test(brush_embed_gate::)'` |
| 30 | `tests/builtin_cli_audit.rs` | `zz_merged_4` | `-E 'test(builtin_cli_audit::)'` |
| 31 | `tests/builtin_only_no_js.rs` | `zz_merged_1` | `-E 'test(builtin_only_no_js::)'` |
| 32 | `tests/catalog_cache.rs` | `zz_merged_5` | `-E 'test(catalog_cache::)'` |
| 33 | `tests/catalog_google_sync.rs` | `zz_merged_3` | `-E 'test(catalog_google_sync::)'` |
| 34 | `tests/catalog_overlay_install.rs` | `zz_merged_1` | `-E 'test(catalog_overlay_install::)'` |
| 35 | `tests/child_process_stdio.rs` | `zz_merged_5` | `-E 'test(child_process_stdio::)'` |
| 36 | `tests/child_process_stream_encoding.rs` | `zz_merged_5` | `-E 'test(child_process_stream_encoding::)'` |
| 37 | `tests/chord_shim.rs` | `zz_merged_5` | `-E 'test(chord_shim::)'` |
| 38 | `tests/claude_env_compat.rs` | `zz_merged_4` | `-E 'test(claude_env_compat::)'` |
| 39 | `tests/cli.rs` | `zz_merged_3` | `-E 'test(cli::)'` |
| 40 | `tests/cli_parity_fixes.rs` | `zz_merged_3` | `-E 'test(cli_parity_fixes::)'` |
| 41 | `tests/client_remove_session.rs` | `zz_merged_1` | `-E 'test(client_remove_session::)'` |
| 42 | `tests/client_rpc.rs` | `zz_merged_1` | `-E 'test(client_rpc::)'` |
| 43 | `tests/client_server.rs` | `zz_merged_4` | `-E 'test(client_server::)'` |
| 44 | `tests/codemode_declarations.rs` | `zz_merged_2` | `-E 'test(codemode_declarations::)'` |
| 45 | `tests/codemode_exposure.rs` | `zz_merged_3` | `-E 'test(codemode_exposure::)'` |
| 46 | `tests/codemode_whitelist.rs` | `zz_merged_5` | `-E 'test(codemode_whitelist::)'` |
| 47 | `tests/codemode_worker_cancel.rs` | `zz_merged_4` | `-E 'test(codemode_worker_cancel::)'` |
| 48 | `tests/coding_agent_shims.rs` | `zz_merged_2` | `-E 'test(coding_agent_shims::)'` |
| 49 | `tests/commit_tracker.rs` | `zz_merged_2` | `-E 'test(commit_tracker::)'` |
| 50 | `tests/compaction_evidence.rs` | `zz_merged_1` | `-E 'test(compaction_evidence::)'` |
| 51 | `tests/console_shim.rs` | `zz_merged_8` | `-E 'test(console_shim::)'` |
| 52 | `tests/constrained_device_budget.rs` | `zz_merged_1` | `-E 'test(constrained_device_budget::)'` |
| 53 | `tests/context_mode.rs` | `zz_merged_3` | `-E 'test(context_mode::)'` |
| 54 | `tests/context_mode_batch.rs` | `zz_merged_1` | `-E 'test(context_mode_batch::)'` |
| 55 | `tests/context_mode_hardening.rs` | `zz_merged_4` | `-E 'test(context_mode_hardening::)'` |
| 56 | `tests/context_mode_hooks.rs` | `zz_merged_3` | `-E 'test(context_mode_hooks::)'` |
| 57 | `tests/context_mode_index.rs` | `zz_merged_3` | `-E 'test(context_mode_index::)'` |
| 58 | `tests/context_mode_proxy.rs` | `zz_merged_8` | `-E 'test(context_mode_proxy::)'` |
| 59 | `tests/core_round2_fixes.rs` | `zz_merged_3` | `-E 'test(core_round2_fixes::)'` |
| 60 | `tests/ctx_actions_and_dialog_timeout.rs` | `zz_merged_1` | `-E 'test(ctx_actions_and_dialog_timeout::)'` |
| 61 | `tests/ctx_ui_parity.rs` | `zz_merged_3` | `-E 'test(ctx_ui_parity::)'` |
| 62 | `tests/d23_reach_consumption.rs` | `zz_merged_3` | `-E 'test(d23_reach_consumption::)'` |
| 63 | `tests/d30_anthropic_error_tool_result_images.rs` | `zz_merged_5` | `-E 'test(d30_anthropic_error_tool_result_images::)'` |
| 64 | `tests/d31_session_share_destination.rs` | `zz_merged_4` | `-E 'test(d31_session_share_destination::)'` |
| 65 | `tests/d32_resume_active_tools.rs` | `zz_merged_5` | `-E 'test(d32_resume_active_tools::)'` |
| 66 | `tests/diagnostics_channel_shims.rs` | `zz_merged_3` | `-E 'test(diagnostics_channel_shims::)'` |
| 67 | `tests/differential_node.rs` | `zz_merged_5` | `-E 'test(differential_node::)'` |
| 68 | `tests/display_parity.rs` | `zz_merged_4` | `-E 'test(display_parity::)'` |
| 69 | `tests/dry_convergence.rs` | `zz_merged_4` | `-E 'test(dry_convergence::)'` |
| 70 | `tests/durable_memcopy_bench.rs` | `durable_memcopy_bench` | `-E 'test(durable_memcopy_bench::)'` |
| 71 | `tests/durable_session_lane_e.rs` | `zz_merged_4` | `-E 'test(durable_session_lane_e::)'` |
| 72 | `tests/durable_shim.rs` | `zz_merged_4` | `-E 'test(durable_shim::)'` |
| 73 | `tests/durable_storage.rs` | `zz_merged_1` | `-E 'test(durable_storage::)'` |
| 74 | `tests/durable_trace_propagation.rs` | `zz_merged_1` | `-E 'test(durable_trace_propagation::)'` |
| 75 | `tests/e2e_cli_agent_session.rs` | `zz_merged_5` | `-E 'test(e2e_cli_agent_session::)'` |
| 76 | `tests/e2e_ollama.rs` | `zz_merged_5` | `-E 'test(e2e_ollama::)'` |
| 77 | `tests/e2e_qwen38max.rs` | `zz_merged_8` | `-E 'test(e2e_qwen38max::)'` |
| 78 | `tests/ecosystem_compat.rs` | `zz_merged_2` | `-E 'test(ecosystem_compat::)'` |
| 79 | `tests/ecosystem_runtime_exec.rs` | `zz_merged_4` | `-E 'test(ecosystem_runtime_exec::)'` |
| 80 | `tests/edit_source_limit.rs` | `zz_merged_1` | `-E 'test(edit_source_limit::)'` |
| 81 | `tests/editor_killring_undo.rs` | `zz_merged_2` | `-E 'test(editor_killring_undo::)'` |
| 82 | `tests/env_client_e2e.rs` | `zz_merged_1` | `-E 'test(env_client_e2e::)'` |
| 83 | `tests/env_client_pty.rs` | `zz_merged_3` | `-E 'test(env_client_pty::)'` |
| 84 | `tests/env_daemon_js_shim.rs` | `zz_merged_3` | `-E 'test(env_daemon_js_shim::)'` |
| 85 | `tests/env_daemon_protocol.rs` | `zz_merged_1` | `-E 'test(env_daemon_protocol::)'` |
| 86 | `tests/extension_background_preemption.rs` | `zz_merged_2` | `-E 'test(extension_background_preemption::)'` |
| 87 | `tests/extension_dispatch_timeout.rs` | `zz_merged_1` | `-E 'test(extension_dispatch_timeout::)'` |
| 88 | `tests/extension_fail_cache.rs` | `zz_merged_2` | `-E 'test(extension_fail_cache::)'` |
| 89 | `tests/extension_timing.rs` | `zz_merged_3` | `-E 'test(extension_timing::)'` |
| 90 | `tests/extensions_longtail.rs` | `zz_merged_3` | `-E 'test(extensions_longtail::)'` |
| 91 | `tests/extensions_relative_entry.rs` | `zz_merged_3` | `-E 'test(extensions_relative_entry::)'` |
| 92 | `tests/extensions_runtime.rs` | `zz_merged_3` | `-E 'test(extensions_runtime::)'` |
| 93 | `tests/feature_off_surface.rs` | `zz_merged_2` | `-E 'test(feature_off_surface::)'` |
| 94 | `tests/find_hidden_files.rs` | `zz_merged_2` | `-E 'test(find_hidden_files::)'` |
| 95 | `tests/footer_provider_count.rs` | `zz_merged_3` | `-E 'test(footer_provider_count::)'` |
| 96 | `tests/fs_numeric_open_flags.rs` | `zz_merged_2` | `-E 'test(fs_numeric_open_flags::)'` |
| 97 | `tests/fs_watch_adaptive.rs` | `zz_merged_4` | `-E 'test(fs_watch_adaptive::)'` |
| 98 | `tests/fs_watch_incremental.rs` | `zz_merged_5` | `-E 'test(fs_watch_incremental::)'` |
| 99 | `tests/fs_watch_real.rs` | `zz_merged_2` | `-E 'test(fs_watch_real::)'` |
| 100 | `tests/g_agent.rs` | `zz_merged_6` | `-E 'test(g_agent::)'` |
| 101 | `tests/g_ai.rs` | `zz_merged_8` | `-E 'test(g_ai::)'` |
| 102 | `tests/g_cli_server.rs` | `zz_merged_4` | `-E 'test(g_cli_server::)'` |
| 103 | `tests/g_durable.rs` | `g_durable` | `-E 'test(g_durable::)'` |
| 104 | `tests/g_extensions.rs` | `zz_merged_3` | `-E 'test(g_extensions::)'` |
| 105 | `tests/g_print_tools_w14.rs` | `zz_merged_1` | `-E 'test(g_print_tools_w14::)'` |
| 106 | `tests/g_protocol.rs` | `zz_merged_2` | `-E 'test(g_protocol::)'` |
| 107 | `tests/g_tools.rs` | `zz_merged_5` | `-E 'test(g_tools::)'` |
| 108 | `tests/g_tui.rs` | `zz_merged_7` | `-E 'test(g_tui::)'` |
| 109 | `tests/git_branch_head_file.rs` | `zz_merged_2` | `-E 'test(git_branch_head_file::)'` |
| 110 | `tests/grep_context_multi_match.rs` | `zz_merged_1` | `-E 'test(grep_context_multi_match::)'` |
| 111 | `tests/hook_coverage.rs` | `zz_merged_4` | `-E 'test(hook_coverage::)'` |
| 112 | `tests/hook_timeout.rs` | `zz_merged_1` | `-E 'test(hook_timeout::)'` |
| 113 | `tests/http_async_resource_context.rs` | `zz_merged_4` | `-E 'test(http_async_resource_context::)'` |
| 114 | `tests/http_body_stream_chunks.rs` | `zz_merged_5` | `-E 'test(http_body_stream_chunks::)'` |
| 115 | `tests/http_poll_pacing.rs` | `zz_merged_3` | `-E 'test(http_poll_pacing::)'` |
| 116 | `tests/http_shims.rs` | `zz_merged_1` | `-E 'test(http_shims::)'` |
| 117 | `tests/idiom_round3.rs` | `zz_merged_3` | `-E 'test(idiom_round3::)'` |
| 118 | `tests/image_attachment_resize.rs` | `zz_merged_5` | `-E 'test(image_attachment_resize::)'` |
| 119 | `tests/image_classifier_models.rs` | `zz_merged_4` | `-E 'test(image_classifier_models::)'` |
| 120 | `tests/image_pipeline_fix.rs` | `zz_merged_4` | `-E 'test(image_pipeline_fix::)'` |
| 121 | `tests/image_resize_perf.rs` | `zz_merged_3` | `-E 'test(image_resize_perf::)'` |
| 122 | `tests/js_stream_incremental.rs` | `zz_merged_3` | `-E 'test(js_stream_incremental::)'` |
| 123 | `tests/json_stream_wire.rs` | `zz_merged_3` | `-E 'test(json_stream_wire::)'` |
| 124 | `tests/lane_e_streamed_widen.rs` | `zz_merged_4` | `-E 'test(lane_e_streamed_widen::)'` |
| 125 | `tests/lane_e_streaming_file_body.rs` | `zz_merged_2` | `-E 'test(lane_e_streaming_file_body::)'` |
| 126 | `tests/lane_event2_project_trust.rs` | `zz_merged_3` | `-E 'test(lane_event2_project_trust::)'` |
| 127 | `tests/lane_event3_json_host.rs` | `zz_merged_2` | `-E 'test(lane_event3_json_host::)'` |
| 128 | `tests/lane_event3_runtime_handoff.rs` | `zz_merged_3` | `-E 'test(lane_event3_runtime_handoff::)'` |
| 129 | `tests/lazy_history.rs` | `zz_merged_2` | `-E 'test(lazy_history::)'` |
| 130 | `tests/lazy_history_memory.rs` | `zz_merged_6` | `-E 'test(lazy_history_memory::)'` |
| 131 | `tests/loader_module_resolution.rs` | `zz_merged_5` | `-E 'test(loader_module_resolution::)'` |
| 132 | `tests/low_resource_units.rs` | `zz_merged_4` | `-E 'test(low_resource_units::)'` |
| 133 | `tests/manifest_cache.rs` | `zz_merged_2` | `-E 'test(manifest_cache::)'` |
| 134 | `tests/mcp_auto_enable.rs` | `zz_merged_3` | `-E 'test(mcp_auto_enable::)'` |
| 135 | `tests/mcp_builtin_tools.rs` | `zz_merged_1` | `-E 'test(mcp_builtin_tools::)'` |
| 136 | `tests/mcp_builtin_tools_e2e.rs` | `zz_merged_5` | `-E 'test(mcp_builtin_tools_e2e::)'` |
| 137 | `tests/mcp_description_lifecycle.rs` | `zz_merged_2` | `-E 'test(mcp_description_lifecycle::)'` |
| 138 | `tests/mcp_frame_cap.rs` | `zz_merged_3` | `-E 'test(mcp_frame_cap::)'` |
| 139 | `tests/mcp_http_body_cap.rs` | `zz_merged_4` | `-E 'test(mcp_http_body_cap::)'` |
| 140 | `tests/mcp_integration.rs` | `zz_merged_1` | `-E 'test(mcp_integration::)'` |
| 141 | `tests/mcp_oauth_store.rs` | `zz_merged_1` | `-E 'test(mcp_oauth_store::)'` |
| 142 | `tests/mcp_singleflight_locks.rs` | `zz_merged_2` | `-E 'test(mcp_singleflight_locks::)'` |
| 143 | `tests/mcp_strict.rs` | `zz_merged_4` | `-E 'test(mcp_strict::)'` |
| 144 | `tests/mcp_units.rs` | `zz_merged_2` | `-E 'test(mcp_units::)'` |
| 145 | `tests/memory_shared.rs` | `zz_merged_2` | `-E 'test(memory_shared::)'` |
| 146 | `tests/message_serde_fix.rs` | `zz_merged_5` | `-E 'test(message_serde_fix::)'` |
| 147 | `tests/minimal_profile.rs` | `zz_merged_5` | `-E 'test(minimal_profile::)'` |
| 148 | `tests/native_cli.rs` | `zz_merged_3` | `-E 'test(native_cli::)'` |
| 149 | `tests/net_shims.rs` | `zz_merged_1` | `-E 'test(net_shims::)'` |
| 150 | `tests/node_shim_injection.rs` | `zz_merged_4` | `-E 'test(node_shim_injection::)'` |
| 151 | `tests/node_shims.rs` | `zz_merged_2` | `-E 'test(node_shims::)'` |
| 152 | `tests/node_surface.rs` | `zz_merged_2` | `-E 'test(node_surface::)'` |
| 153 | `tests/node_tests_wave_b.rs` | `zz_merged_5` | `-E 'test(node_tests_wave_b::)'` |
| 154 | `tests/npm_modules.rs` | `zz_merged_1` | `-E 'test(npm_modules::)'` |
| 155 | `tests/oauth_page_and_load.rs` | `zz_merged_2` | `-E 'test(oauth_page_and_load::)'` |
| 156 | `tests/oauth_tui_login.rs` | `zz_merged_4` | `-E 'test(oauth_tui_login::)'` |
| 157 | `tests/only_builtin_flag.rs` | `zz_merged_1` | `-E 'test(only_builtin_flag::)'` |
| 158 | `tests/openai_model_max_tokens.rs` | `zz_merged_5` | `-E 'test(openai_model_max_tokens::)'` |
| 159 | `tests/openai_responses_parity.rs` | `zz_merged_2` | `-E 'test(openai_responses_parity::)'` |
| 160 | `tests/optimization_parity.rs` | `zz_merged_4` | `-E 'test(optimization_parity::)'` |
| 161 | `tests/osc11_unix_reply_reader.rs` | `zz_merged_3` | `-E 'test(osc11_unix_reply_reader::)'` |
| 162 | `tests/p0_checkpoints.rs` | `zz_merged_4` | `-E 'test(p0_checkpoints::)'` |
| 163 | `tests/p0_jobs.rs` | `zz_merged_4` | `-E 'test(p0_jobs::)'` |
| 164 | `tests/p0_session_retention.rs` | `zz_merged_1` | `-E 'test(p0_session_retention::)'` |
| 165 | `tests/p0_token_estimator.rs` | `zz_merged_1` | `-E 'test(p0_token_estimator::)'` |
| 166 | `tests/p2_fixes.rs` | `zz_merged_4` | `-E 'test(p2_fixes::)'` |
| 167 | `tests/p2_inbox_toctou.rs` | `zz_merged_4` | `-E 'test(p2_inbox_toctou::)'` |
| 168 | `tests/paged_session_consumers.rs` | `zz_merged_4` | `-E 'test(paged_session_consumers::)'` |
| 169 | `tests/parallel_tool_streaming.rs` | `zz_merged_3` | `-E 'test(parallel_tool_streaming::)'` |
| 170 | `tests/partial_mirror.rs` | `zz_merged_8` | `-E 'test(partial_mirror::)'` |
| 171 | `tests/phone_builtin_replaceable.rs` | `zz_merged_3` | `-E 'test(phone_builtin_replaceable::)'` |
| 172 | `tests/phone_child_inline.rs` | `zz_merged_1` | `-E 'test(phone_child_inline::)'` |
| 173 | `tests/phone_live_session.rs` | `zz_merged_2` | `-E 'test(phone_live_session::)'` |
| 174 | `tests/phone_session_bridge.rs` | `zz_merged_4` | `-E 'test(phone_session_bridge::)'` |
| 175 | `tests/phone_session_unit.rs` | `zz_merged_4` | `-E 'test(phone_session_unit::)'` |
| 176 | `tests/pi_codemode_host_route.rs` | `zz_merged_3` | `-E 'test(pi_codemode_host_route::)'` |
| 177 | `tests/pi_messages_stream_fix.rs` | `zz_merged_2` | `-E 'test(pi_messages_stream_fix::)'` |
| 178 | `tests/pi_phone_default_disabled.rs` | `zz_merged_5` | `-E 'test(pi_phone_default_disabled::)'` |
| 179 | `tests/pi_shims.rs` | `zz_merged_4` | `-E 'test(pi_shims::)'` |
| 180 | `tests/pir_js_runner.rs` | `zz_merged_4` | `-E 'test(pir_js_runner::)'` |
| 181 | `tests/plan_only_session_stream_cell.rs` | `zz_merged_8` | `-E 'test(plan_only_session_stream_cell::)'` |
| 182 | `tests/plugin_context.rs` | `zz_merged_2` | `-E 'test(plugin_context::)'` |
| 183 | `tests/plugin_events.rs` | `zz_merged_4` | `-E 'test(plugin_events::)'` |
| 184 | `tests/plugin_fanout_wiring.rs` | `zz_merged_4` | `-E 'test(plugin_fanout_wiring::)'` |
| 185 | `tests/plugin_gates.rs` | `zz_merged_1` | `-E 'test(plugin_gates::)'` |
| 186 | `tests/plugin_host_statuses.rs` | `zz_merged_1` | `-E 'test(plugin_host_statuses::)'` |
| 187 | `tests/plugin_live_services.rs` | `zz_merged_1` | `-E 'test(plugin_live_services::)'` |
| 188 | `tests/plugin_message_tool_ui.rs` | `zz_merged_5` | `-E 'test(plugin_message_tool_ui::)'` |
| 189 | `tests/plugin_provider_auth.rs` | `zz_merged_4` | `-E 'test(plugin_provider_auth::)'` |
| 190 | `tests/plugin_provider_backend.rs` | `zz_merged_1` | `-E 'test(plugin_provider_backend::)'` |
| 191 | `tests/plugin_renderer_completion.rs` | `zz_merged_1` | `-E 'test(plugin_renderer_completion::)'` |
| 192 | `tests/plugin_service_perf.rs` | `zz_merged_2` | `-E 'test(plugin_service_perf::)'` |
| 193 | `tests/plugin_services_hardening.rs` | `zz_merged_4` | `-E 'test(plugin_services_hardening::)'` |
| 194 | `tests/plugin_session_actions.rs` | `zz_merged_1` | `-E 'test(plugin_session_actions::)'` |
| 195 | `tests/plugin_ui_dialogs.rs` | `zz_merged_2` | `-E 'test(plugin_ui_dialogs::)'` |
| 196 | `tests/plugin_widget_factories.rs` | `zz_merged_3` | `-E 'test(plugin_widget_factories::)'` |
| 197 | `tests/plugins_phone_hardening.rs` | `zz_merged_3` | `-E 'test(plugins_phone_hardening::)'` |
| 198 | `tests/print_lifecycle_hooks.rs` | `zz_merged_3` | `-E 'test(print_lifecycle_hooks::)'` |
| 199 | `tests/print_native_session.rs` | `print_native_session` | `-E 'test(print_native_session::)'` |
| 200 | `tests/print_tool_call_wait.rs` | `zz_merged_2` | `-E 'test(print_tool_call_wait::)'` |
| 201 | `tests/print_tool_update_json.rs` | `zz_merged_1` | `-E 'test(print_tool_update_json::)'` |
| 202 | `tests/process_beforeexit.rs` | `zz_merged_2` | `-E 'test(process_beforeexit::)'` |
| 203 | `tests/prompt_path_fixes.rs` | `zz_merged_5` | `-E 'test(prompt_path_fixes::)'` |
| 204 | `tests/provider_catalog_shared.rs` | `zz_merged_8` | `-E 'test(provider_catalog_shared::)'` |
| 205 | `tests/provider_config_fixes.rs` | `zz_merged_2` | `-E 'test(provider_config_fixes::)'` |
| 206 | `tests/provider_registration_parity.rs` | `zz_merged_5` | `-E 'test(provider_registration_parity::)'` |
| 207 | `tests/provider_shared_context.rs` | `zz_merged_4` | `-E 'test(provider_shared_context::)'` |
| 208 | `tests/python_builtin_audit.rs` | `zz_merged_1` | `-E 'test(python_builtin_audit::)'` |
| 209 | `tests/python_json_fastpath.rs` | `zz_merged_5` | `-E 'test(python_json_fastpath::)'` |
| 210 | `tests/python_plugin.rs` | `zz_merged_1` | `-E 'test(python_plugin::)'` |
| 211 | `tests/python_session_lifecycle.rs` | `python_session_lifecycle` | `-E 'test(python_session_lifecycle::)'` |
| 212 | `tests/read_before_edit.rs` | `zz_merged_4` | `-E 'test(read_before_edit::)'` |
| 213 | `tests/render_diff.rs` | `zz_merged_2` | `-E 'test(render_diff::)'` |
| 214 | `tests/renderer_in_runtime_fix.rs` | `zz_merged_3` | `-E 'test(renderer_in_runtime_fix::)'` |
| 215 | `tests/renderer_parity.rs` | `zz_merged_1` | `-E 'test(renderer_parity::)'` |
| 216 | `tests/renderer_ui_guard.rs` | `zz_merged_2` | `-E 'test(renderer_ui_guard::)'` |
| 217 | `tests/rpc_bash_record_fix.rs` | `zz_merged_1` | `-E 'test(rpc_bash_record_fix::)'` |
| 218 | `tests/rpc_export_off.rs` | `zz_merged_5` | `-E 'test(rpc_export_off::)'` |
| 219 | `tests/rss_a_entry_append_materialization.rs` | `zz_merged_8` | `-E 'test(rss_a_entry_append_materialization::)'` |
| 220 | `tests/rss_b_incremental_projection.rs` | `zz_merged_5` | `-E 'test(rss_b_incremental_projection::)'` |
| 221 | `tests/sdk_agent_session_errors.rs` | `zz_merged_5` | `-E 'test(sdk_agent_session_errors::)'` |
| 222 | `tests/sdk_consumer_embed.rs` | `zz_merged_8` | `-E 'test(sdk_consumer_embed::)'` |
| 223 | `tests/sdk_consumer_lane_f.rs` | `zz_merged_5` | `-E 'test(sdk_consumer_lane_f::)'` |
| 224 | `tests/sdk_consumer_surface.rs` | `zz_merged_1` | `-E 'test(sdk_consumer_surface::)'` |
| 225 | `tests/sdk_extended.rs` | `zz_merged_1` | `-E 'test(sdk_extended::)'` |
| 226 | `tests/sdk_extensibility.rs` | `zz_merged_4` | `-E 'test(sdk_extensibility::)'` |
| 227 | `tests/security_fixes.rs` | `zz_merged_2` | `-E 'test(security_fixes::)'` |
| 228 | `tests/server_cancel_fix.rs` | `zz_merged_2` | `-E 'test(server_cancel_fix::)'` |
| 229 | `tests/server_session_closeout.rs` | `zz_merged_3` | `-E 'test(server_session_closeout::)'` |
| 230 | `tests/session_append_durability.rs` | `zz_merged_5` | `-E 'test(session_append_durability::)'` |
| 231 | `tests/session_build_context.rs` | `zz_merged_1` | `-E 'test(session_build_context::)'` |
| 232 | `tests/session_context_cli.rs` | `zz_merged_2` | `-E 'test(session_context_cli::)'` |
| 233 | `tests/session_context_snapshot_cache.rs` | `zz_merged_4` | `-E 'test(session_context_snapshot_cache::)'` |
| 234 | `tests/session_duplicate_writer.rs` | `zz_merged_4` | `-E 'test(session_duplicate_writer::)'` |
| 235 | `tests/session_listing_single_read.rs` | `zz_merged_5` | `-E 'test(session_listing_single_read::)'` |
| 236 | `tests/session_lock_scopes.rs` | `zz_merged_2` | `-E 'test(session_lock_scopes::)'` |
| 237 | `tests/session_manager_ext.rs` | `zz_merged_3` | `-E 'test(session_manager_ext::)'` |
| 238 | `tests/session_manager_scalar_accessors.rs` | `zz_merged_5` | `-E 'test(session_manager_scalar_accessors::)'` |
| 239 | `tests/session_stats_virtual_round_trip.rs` | `zz_merged_1` | `-E 'test(session_stats_virtual_round_trip::)'` |
| 240 | `tests/settings_diagnostics.rs` | `zz_merged_1` | `-E 'test(settings_diagnostics::)'` |
| 241 | `tests/shell_command_timeout.rs` | `zz_merged_2` | `-E 'test(shell_command_timeout::)'` |
| 242 | `tests/shell_pool.rs` | `zz_merged_1` | `-E 'test(shell_pool::)'` |
| 243 | `tests/shell_pool_prewarm.rs` | `zz_merged_3` | `-E 'test(shell_pool_prewarm::)'` |
| 244 | `tests/shell_totals_fix.rs` | `zz_merged_3` | `-E 'test(shell_totals_fix::)'` |
| 245 | `tests/slash_and_scope_parity.rs` | `zz_merged_5` | `-E 'test(slash_and_scope_parity::)'` |
| 246 | `tests/spawn_sync_stdio.rs` | `zz_merged_1` | `-E 'test(spawn_sync_stdio::)'` |
| 247 | `tests/spill_file_cap.rs` | `zz_merged_3` | `-E 'test(spill_file_cap::)'` |
| 248 | `tests/sqlite_shims.rs` | `zz_merged_2` | `-E 'test(sqlite_shims::)'` |
| 249 | `tests/stream_persist_fixes.rs` | `zz_merged_3` | `-E 'test(stream_persist_fixes::)'` |
| 250 | `tests/stream_shim_contract.rs` | `zz_merged_4` | `-E 'test(stream_shim_contract::)'` |
| 251 | `tests/subagent_budget.rs` | `zz_merged_5` | `-E 'test(subagent_budget::)'` |
| 252 | `tests/subagent_builtin.rs` | `zz_merged_2` | `-E 'test(subagent_builtin::)'` |
| 253 | `tests/subagent_cli_parallel.rs` | `zz_merged_5` | `-E 'test(subagent_cli_parallel::)'` |
| 254 | `tests/subagent_cli_print.rs` | `zz_merged_8` | `-E 'test(subagent_cli_print::)'` |
| 255 | `tests/subagent_depth.rs` | `zz_merged_4` | `-E 'test(subagent_depth::)'` |
| 256 | `tests/subagent_model_catalog.rs` | `zz_merged_3` | `-E 'test(subagent_model_catalog::)'` |
| 257 | `tests/subagent_runtime_isolation.rs` | `zz_merged_4` | `-E 'test(subagent_runtime_isolation::)'` |
| 258 | `tests/subagents_parent_launch.rs` | `zz_merged_2` | `-E 'test(subagents_parent_launch::)'` |
| 259 | `tests/telemetry_ai_cost_w17.rs` | `zz_merged_2` | `-E 'test(telemetry_ai_cost_w17::)'` |
| 260 | `tests/telemetry_cache.rs` | `zz_merged_3` | `-E 'test(telemetry_cache::)'` |
| 261 | `tests/telemetry_store_w17.rs` | `zz_merged_2` | `-E 'test(telemetry_store_w17::)'` |
| 262 | `tests/terminal_input_hooks.rs` | `zz_merged_2` | `-E 'test(terminal_input_hooks::)'` |
| 263 | `tests/theme_once_semantics.rs` | `zz_merged_1` | `-E 'test(theme_once_semantics::)'` |
| 264 | `tests/tool_event_json_payloads.rs` | `zz_merged_2` | `-E 'test(tool_event_json_payloads::)'` |
| 265 | `tests/tool_registration_fix.rs` | `zz_merged_3` | `-E 'test(tool_registration_fix::)'` |
| 266 | `tests/tool_registry_fingerprint.rs` | `zz_merged_2` | `-E 'test(tool_registry_fingerprint::)'` |
| 267 | `tests/tool_search_declaration.rs` | `zz_merged_1` | `-E 'test(tool_search_declaration::)'` |
| 268 | `tests/tools_feature_off.rs` | `zz_merged_4` | `-E 'test(tools_feature_off::)'` |
| 269 | `tests/tree_depth_safety.rs` | `zz_merged_5` | `-E 'test(tree_depth_safety::)'` |
| 270 | `tests/truncate_tail_edge.rs` | `zz_merged_3` | `-E 'test(truncate_tail_edge::)'` |
| 271 | `tests/trust_p0_lane_f.rs` | `zz_merged_2` | `-E 'test(trust_p0_lane_f::)'` |
| 272 | `tests/tui_100k_frame.rs` | `zz_merged_4` | `-E 'test(tui_100k_frame::)'` |
| 273 | `tests/tui_audit_lane_f.rs` | `zz_merged_3` | `-E 'test(tui_audit_lane_f::)'` |
| 274 | `tests/tui_autocomplete_accept.rs` | `zz_merged_5` | `-E 'test(tui_autocomplete_accept::)'` |
| 275 | `tests/tui_autocomplete_dynamic.rs` | `zz_merged_5` | `-E 'test(tui_autocomplete_dynamic::)'` |
| 276 | `tests/tui_bash_mode.rs` | `zz_merged_4` | `-E 'test(tui_bash_mode::)'` |
| 277 | `tests/tui_bugfix_batch.rs` | `zz_merged_2` | `-E 'test(tui_bugfix_batch::)'` |
| 278 | `tests/tui_capability_overrides.rs` | `zz_merged_3` | `-E 'test(tui_capability_overrides::)'` |
| 279 | `tests/tui_command_sources.rs` | `zz_merged_1` | `-E 'test(tui_command_sources::)'` |
| 280 | `tests/tui_commands_parity.rs` | `zz_merged_4` | `-E 'test(tui_commands_parity::)'` |
| 281 | `tests/tui_crash_log_attribution.rs` | `zz_merged_2` | `-E 'test(tui_crash_log_attribution::)'` |
| 282 | `tests/tui_debug_copy_search_w17.rs` | `zz_merged_2` | `-E 'test(tui_debug_copy_search_w17::)'` |
| 283 | `tests/tui_dirty_refresh.rs` | `zz_merged_2` | `-E 'test(tui_dirty_refresh::)'` |
| 284 | `tests/tui_drift_w1.rs` | `zz_merged_3` | `-E 'test(tui_drift_w1::)'` |
| 285 | `tests/tui_emoji_first.rs` | `zz_merged_2` | `-E 'test(tui_emoji_first::)'` |
| 286 | `tests/tui_emoji_input.rs` | `zz_merged_5` | `-E 'test(tui_emoji_input::)'` |
| 287 | `tests/tui_escape_sanitize.rs` | `zz_merged_3` | `-E 'test(tui_escape_sanitize::)'` |
| 288 | `tests/tui_fixes_round2.rs` | `zz_merged_3` | `-E 'test(tui_fixes_round2::)'` |
| 289 | `tests/tui_frame_reuse.rs` | `zz_merged_2` | `-E 'test(tui_frame_reuse::)'` |
| 290 | `tests/tui_image_preview_off.rs` | `zz_merged_4` | `-E 'test(tui_image_preview_off::)'` |
| 291 | `tests/tui_ime_regression.rs` | `zz_merged_1` | `-E 'test(tui_ime_regression::)'` |
| 292 | `tests/tui_input_order.rs` | `zz_merged_1` | `-E 'test(tui_input_order::)'` |
| 293 | `tests/tui_keybinding_namespace.rs` | `zz_merged_1` | `-E 'test(tui_keybinding_namespace::)'` |
| 294 | `tests/tui_loop_fixes.rs` | `zz_merged_1` | `-E 'test(tui_loop_fixes::)'` |
| 295 | `tests/tui_mcp_stack_overflow.rs` | `zz_merged_5` | `-E 'test(tui_mcp_stack_overflow::)'` |
| 296 | `tests/tui_model_names.rs` | `zz_merged_1` | `-E 'test(tui_model_names::)'` |
| 297 | `tests/tui_model_selector_configured.rs` | `zz_merged_3` | `-E 'test(tui_model_selector_configured::)'` |
| 298 | `tests/tui_native_commands.rs` | `zz_merged_1` | `-E 'test(tui_native_commands::)'` |
| 299 | `tests/tui_paged_load_more.rs` | `zz_merged_1` | `-E 'test(tui_paged_load_more::)'` |
| 300 | `tests/tui_paste_burst_p5.rs` | `zz_merged_2` | `-E 'test(tui_paste_burst_p5::)'` |
| 301 | `tests/tui_paste_perf.rs` | `zz_merged_5` | `-E 'test(tui_paste_perf::)'` |
| 302 | `tests/tui_quickedit_mode.rs` | `zz_merged_2` | `-E 'test(tui_quickedit_mode::)'` |
| 303 | `tests/tui_render_parity.rs` | `zz_merged_2` | `-E 'test(tui_render_parity::)'` |
| 304 | `tests/tui_search_whole_history.rs` | `zz_merged_3` | `-E 'test(tui_search_whole_history::)'` |
| 305 | `tests/tui_session_event_frames.rs` | `zz_merged_1` | `-E 'test(tui_session_event_frames::)'` |
| 306 | `tests/tui_session_reset.rs` | `zz_merged_5` | `-E 'test(tui_session_reset::)'` |
| 307 | `tests/tui_shims.rs` | `zz_merged_4` | `-E 'test(tui_shims::)'` |
| 308 | `tests/tui_slash_model_argument.rs` | `zz_merged_5` | `-E 'test(tui_slash_model_argument::)'` |
| 309 | `tests/tui_snapshot_refresh_w16.rs` | `zz_merged_1` | `-E 'test(tui_snapshot_refresh_w16::)'` |
| 310 | `tests/tui_thinking_footer.rs` | `zz_merged_5` | `-E 'test(tui_thinking_footer::)'` |
| 311 | `tests/tui_thinking_persist.rs` | `zz_merged_1` | `-E 'test(tui_thinking_persist::)'` |
| 312 | `tests/tui_toolcall_args_stream.rs` | `zz_merged_5` | `-E 'test(tui_toolcall_args_stream::)'` |
| 313 | `tests/tui_windowed_transcript.rs` | `zz_merged_4` | `-E 'test(tui_windowed_transcript::)'` |
| 314 | `tests/unsafe_gate.rs` | `zz_merged_3` | `-E 'test(unsafe_gate::)'` |
| 315 | `tests/upstream_compaction_summary_limits.rs` | `zz_merged_4` | `-E 'test(upstream_compaction_summary_limits::)'` |
| 316 | `tests/upstream_d34_retry.rs` | `zz_merged_3` | `-E 'test(upstream_d34_retry::)'` |
| 317 | `tests/upstream_d35_tool_output.rs` | `zz_merged_4` | `-E 'test(upstream_d35_tool_output::)'` |
| 318 | `tests/upstream_d36_thinking.rs` | `zz_merged_4` | `-E 'test(upstream_d36_thinking::)'` |
| 319 | `tests/upstream_d3_strict_tool_fallback.rs` | `zz_merged_2` | `-E 'test(upstream_d3_strict_tool_fallback::)'` |
| 320 | `tests/upstream_env_protocol.rs` | `zz_merged_1` | `-E 'test(upstream_env_protocol::)'` |
| 321 | `tests/upstream_prompt_templates_args.rs` | `zz_merged_1` | `-E 'test(upstream_prompt_templates_args::)'` |
| 322 | `tests/upstream_w1_drift.rs` | `zz_merged_2` | `-E 'test(upstream_w1_drift::)'` |
| 323 | `tests/upstream_w2_drift.rs` | `zz_merged_4` | `-E 'test(upstream_w2_drift::)'` |
| 324 | `tests/version_consistency.rs` | `zz_merged_2` | `-E 'test(version_consistency::)'` |
| 325 | `tests/w1_d12_openrouter.rs` | `zz_merged_4` | `-E 'test(w1_d12_openrouter::)'` |
| 326 | `tests/w1_d7_tilde_patterns.rs` | `zz_merged_3` | `-E 'test(w1_d7_tilde_patterns::)'` |
| 327 | `tests/w1_d8_mcp.rs` | `zz_merged_4` | `-E 'test(w1_d8_mcp::)'` |
| 328 | `tests/web_globals.rs` | `zz_merged_5` | `-E 'test(web_globals::)'` |
| 329 | `tests/widget_factories.rs` | `zz_merged_2` | `-E 'test(widget_factories::)'` |
| 330 | `tests/worker_threads_real.rs` | `worker_threads_real` | `-E 'test(worker_threads_real::)'` |
| 331 | `tests/zlib_crypto_shims.rs` | `zz_merged_4` | `-E 'test(zlib_crypto_shims::)'` |

### Shadowed allocator sources

| shadowed source | built instead as | harness target |
|---|---|---|
| `tests/compaction_peak_attribution.rs` | `tests/zz_shadow_compaction_peak_attribution.rs` | `zz_merged_8` |
| `tests/lane_e_streamed_peak.rs` | `tests/zz_shadow_lane_e_streamed_peak.rs` | `zz_merged_8` |
| `tests/lane_e_widen_attribution.rs` | `tests/zz_shadow_lane_e_widen_attribution.rs` | `zz_merged_8` |
| `tests/sdk_heap_profile.rs` | `tests/zz_shadow_sdk_heap_profile.rs` | `zz_merged_8` |
| `tests/session_projection_heap.rs` | `tests/zz_shadow_session_projection_heap.rs` | `zz_merged_8` |
| `tests/startup_heap_profile.rs` | `tests/zz_shadow_startup_heap_profile.rs` | `zz_merged_8` |

### Composite sources pulled in with `include!`

`tests/fragments/sdk_round13_on_event.rs` is textually pulled into
`tests/sdk_extended.rs` with `include!`, so its tests compile into the harness
that owns `sdk_extended` and it is not a target of its own.
