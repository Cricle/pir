//! Standalone harness for the TUI session-root refresh suite.
//!
//! This suite reads the process-global `session_list_file_reads` probe. It was
//! merged into `g_tui`, but another suite there also calls
//! `SessionManager::list_sessions`, so the probe could observe a concurrent
//! read. Keep it in its own process; `g_tui` must not re-export this module.

#[path = "suites/tui_snapshot_refresh_w16.rs"]
mod tui_snapshot_refresh_w16;
