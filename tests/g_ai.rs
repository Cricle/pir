// Merged test binary: one link unit for a whole subsystem (test-suite link
// time dominates `cargo test`; fewer binaries = faster compile). Each file
// keeps its own module namespace, so helpers cannot collide.
#[path = "suites/ai_abort_partial_w13.rs"]
mod ai_abort_partial_w13;
#[path = "suites/ai_abort_signal.rs"]
mod ai_abort_signal;
#[path = "suites/ai_adapter_fixes.rs"]
mod ai_adapter_fixes;
#[path = "suites/ai_adapter_parity.rs"]
mod ai_adapter_parity;
#[path = "suites/ai_adapters_parity.rs"]
mod ai_adapters_parity;
#[path = "suites/ai_adapters_w11.rs"]
mod ai_adapters_w11;
#[path = "suites/ai_auth.rs"]
mod ai_auth;
#[path = "suites/ai_bedrock_credentials.rs"]
mod ai_bedrock_credentials;
#[path = "suites/ai_catalog_full_w10.rs"]
mod ai_catalog_full_w10;
#[path = "suites/ai_catalog_w10.rs"]
mod ai_catalog_w10;
#[path = "suites/ai_codex_ws_w10.rs"]
mod ai_codex_ws_w10;
#[path = "suites/ai_codex_ws_w13.rs"]
mod ai_codex_ws_w13;
#[path = "suites/ai_copilot_headers.rs"]
mod ai_copilot_headers;
#[path = "suites/ai_error.rs"]
mod ai_error;
#[path = "suites/ai_event_stream.rs"]
mod ai_event_stream;
#[path = "suites/ai_event_stream_backpressure.rs"]
mod ai_event_stream_backpressure;
#[path = "suites/ai_event_stream_drop_w13.rs"]
mod ai_event_stream_drop_w13;
#[path = "suites/ai_event_stream_ext.rs"]
mod ai_event_stream_ext;
#[path = "suites/ai_google.rs"]
mod ai_google;
#[path = "suites/ai_http.rs"]
mod ai_http;
#[path = "suites/ai_malformed_args_w10.rs"]
mod ai_malformed_args_w10;
#[path = "suites/ai_pi_messages_cow.rs"]
mod ai_pi_messages_cow;
#[path = "suites/ai_producer_liveness_w13.rs"]
mod ai_producer_liveness_w13;
#[path = "suites/ai_provider.rs"]
mod ai_provider;
#[path = "suites/ai_provider_contracts.rs"]
mod ai_provider_contracts;
#[path = "suites/ai_remaining_adapter_contracts.rs"]
mod ai_remaining_adapter_contracts;
#[path = "suites/ai_responses_tool_changes.rs"]
mod ai_responses_tool_changes;
#[path = "suites/ai_retry_overflow_d2.rs"]
mod ai_retry_overflow_d2;
#[path = "suites/ai_sse.rs"]
mod ai_sse;
#[path = "suites/ai_sse_proptest.rs"]
mod ai_sse_proptest;
#[path = "suites/ai_telemetry_w11.rs"]
mod ai_telemetry_w11;
#[path = "suites/ai_transcript.rs"]
mod ai_transcript;
#[path = "suites/ai_transport_idle.rs"]
mod ai_transport_idle;
#[path = "suites/ai_types.rs"]
mod ai_types;
#[path = "suites/ai_util.rs"]
mod ai_util;
#[path = "suites/auth_cli.rs"]
mod auth_cli;
#[path = "suites/auth_oauth.rs"]
mod auth_oauth;
#[path = "suites/auth_oauth_unit.rs"]
mod auth_oauth_unit;
#[path = "suites/builtins_cross.rs"]
mod builtins_cross;
#[path = "suites/catalog_drift.rs"]
mod catalog_drift;
#[path = "suites/catalog_units.rs"]
mod catalog_units;
#[path = "common/mod.rs"]
mod common;
#[path = "suites/config_cache_lifetime_w16.rs"]
mod config_cache_lifetime_w16;
#[path = "suites/g11_provider_attribution.rs"]
mod g11_provider_attribution;
#[path = "suites/globset_w11.rs"]
mod globset_w11;
#[path = "suites/model_catalog.rs"]
mod model_catalog;
#[path = "suites/model_catalog_strict.rs"]
mod model_catalog_strict;
#[path = "suites/model_resolution_units.rs"]
mod model_resolution_units;
#[path = "suites/pi_config_trust.rs"]
mod pi_config_trust;
#[path = "suites/provider_config_units.rs"]
mod provider_config_units;
#[path = "suites/quality_type_modeling.rs"]
mod quality_type_modeling;
#[path = "suites/schema_catalog_w11.rs"]
mod schema_catalog_w11;
#[path = "suites/telemetry_core_w10.rs"]
mod telemetry_core_w10;
