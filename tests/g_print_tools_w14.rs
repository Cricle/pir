//! W14-G: end-to-end print-mode tool exposure asserted on the wire.
//!
//! `tests/suites/*.rs` is not auto-discovered by cargo: each suite is a module
//! of a merged `g_*` test binary (see `tests/g_agent.rs`'s header comment).
//! This dedicated root keeps the process-spawning suite in its own binary —
//! its mock provider and its `pir print` children must not share a process
//! with the unit suites — and gives the foreman an exact target:
//! `cargo test --test g_print_tools_w14`.
#[path = "suites/print_request_tools_w14.rs"]
mod print_request_tools_w14;
