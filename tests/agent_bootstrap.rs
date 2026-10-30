//! Merged test binary: startup bootstrap of the pi-compatible agent directory.
//!
//! Kept as its own (small) link unit so the bootstrap regressions do not pull
//! in the entire `g_agent`/`g_cli_server` suite for a targeted run.
mod common;

#[path = "suites/agent_bootstrap.rs"]
mod agent_bootstrap;
