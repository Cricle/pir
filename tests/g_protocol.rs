// Merged test binary: one link unit for a whole subsystem (test-suite link
// time dominates `cargo test`; fewer binaries = faster compile). Each file
// keeps its own module namespace, so helpers cannot collide.
//
// The pure wire-protocol unit suites (`cbor`, `framing`) moved to
// `crates/pir-protocol/tests/` with the crate. `suites/protocol.rs` stays here:
// it is an integration suite that drives `ai` + the server, not a protocol
// unit test.
#[path = "suites/config_migrations.rs"]
mod config_migrations;
#[path = "suites/config_schemas.rs"]
mod config_schemas;
#[path = "suites/config_settings_units.rs"]
mod config_settings_units;
#[path = "suites/config_units.rs"]
mod config_units;
#[path = "suites/forbidden_files.rs"]
mod forbidden_files;
#[path = "suites/lib_exports.rs"]
mod lib_exports;
#[path = "suites/orchestration_convergence.rs"]
mod orchestration_convergence;
#[path = "suites/production_safety.rs"]
mod production_safety;
#[path = "suites/protocol.rs"]
mod protocol;
#[path = "suites/schema_generator_w11.rs"]
mod schema_generator_w11;
#[path = "suites/source_hygiene.rs"]
mod source_hygiene;
#[path = "suites/upstream_examples.rs"]
mod upstream_examples;
