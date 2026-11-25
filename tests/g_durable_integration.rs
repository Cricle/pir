// Durable integration suites that need the engine's real `SessionManager` /
// extension runtime, so they cannot live in `crates/pir-durable/tests/` (that
// crate has no dependency on `pir-core`). The pure durable suites moved to the
// `pir-durable` crate; these three stay here, linked against `pir-sdk`.
#[path = "suites/durable_residuals_w11.rs"]
mod durable_residuals_w11;
#[path = "suites/pi_format_export.rs"]
mod pi_format_export;
#[path = "suites/pi_format_roundtrip.rs"]
mod pi_format_roundtrip;
