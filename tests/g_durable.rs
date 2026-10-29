// Durable runtime test binary.
//
// One link unit for the durable suites (the session kernel, harness, built-in
// tasks and the pi-format bridge). Each file keeps its own module namespace.
// Suites are registered here as each lane lands; the file exists for every
// module it names, so a lane's `cargo check --test g_durable` sees only its own
// scope.
#[path = "suites/durable_assemble.rs"]
mod durable_assemble;
#[path = "suites/durable_chord_diff.rs"]
mod durable_chord_diff;
#[path = "suites/durable_context.rs"]
mod durable_context;
#[path = "suites/durable_definitions.rs"]
mod durable_definitions;
#[path = "suites/durable_delta.rs"]
mod durable_delta;

#[path = "suites/durable_attach.rs"]
mod durable_attach;
#[path = "suites/durable_facade.rs"]
mod durable_facade;

#[path = "suites/durable_harness_agent.rs"]
mod durable_harness_agent;
#[path = "suites/durable_harness_settings.rs"]
mod durable_harness_settings;

#[path = "suites/durable_inbox.rs"]
mod durable_inbox;

#[path = "suites/durable_context_derive.rs"]
mod durable_context_derive;
#[path = "suites/durable_prompt.rs"]
mod durable_prompt;
#[path = "suites/durable_scheduler.rs"]
mod durable_scheduler;

#[path = "suites/durable_builtin_documents.rs"]
mod durable_builtin_documents;
#[path = "suites/durable_publication.rs"]
mod durable_publication;

#[path = "suites/durable_compaction.rs"]
mod durable_compaction;
#[path = "suites/durable_document_cache.rs"]
mod durable_document_cache;
#[path = "suites/durable_forks.rs"]
mod durable_forks;
#[path = "suites/durable_generation.rs"]
mod durable_generation;
#[path = "suites/durable_guard.rs"]
mod durable_guard;
#[path = "suites/durable_json.rs"]
mod durable_json;
#[path = "suites/durable_lifecycle_w8.rs"]
mod durable_lifecycle_w8;
#[path = "suites/durable_live.rs"]
mod durable_live;
#[path = "suites/durable_observation_units.rs"]
mod durable_observation_units;
#[path = "suites/durable_observer.rs"]
mod durable_observer;
#[path = "suites/durable_primitives.rs"]
mod durable_primitives;
#[path = "suites/durable_registry.rs"]
mod durable_registry;
#[path = "suites/durable_sqlite_seam.rs"]
mod durable_sqlite_seam;
#[path = "suites/durable_storage_conformance.rs"]
mod durable_storage_conformance;
#[path = "suites/durable_task_definition.rs"]
mod durable_task_definition;
#[path = "suites/durable_transaction.rs"]
mod durable_transaction;
#[path = "suites/durable_usage.rs"]
mod durable_usage;
#[path = "suites/pi_format_export.rs"]
mod pi_format_export;
#[path = "suites/pi_format_import.rs"]
mod pi_format_import;
#[path = "suites/pi_format_roundtrip.rs"]
mod pi_format_roundtrip;

#[path = "suites/durable_harness_documents.rs"]
mod durable_harness_documents;
#[path = "suites/durable_submissions.rs"]
mod durable_submissions;
#[path = "suites/durable_util.rs"]
mod durable_util;
#[path = "suites/durable_view.rs"]
mod durable_view;

#[path = "suites/durable_events.rs"]
mod durable_events;

#[path = "suites/durable_harness.rs"]
mod durable_harness;

#[path = "suites/durable_tool.rs"]
mod durable_tool;

#[path = "suites/durable_task_graph.rs"]
mod durable_task_graph;

#[path = "suites/durable_trace_w8.rs"]
mod durable_trace_w8;

#[path = "suites/durable_residuals_w11.rs"]
mod durable_residuals_w11;
