//! Session-side accessors for the per-run diagnostics summary
//! (`crate::diagnostics`).
//!
//! The collector itself lives in `src/agent_session/prompt_loop.rs`: every
//! run wraps the host's event handler so the session's own recording rides
//! the events the loop already emits, and stores the finished summary here.
//! Hosts read it back with [`AgentSession::last_run_diagnostics`] — a
//! follow-up TUI lane can render it (e.g. a `/diagnostics` command) without
//! touching the loop or the collector.

use super::*;

impl AgentSession {
    /// The most recently finished run's diagnostics summary: the per-turn
    /// provider wait / stream / tool / token breakdown assembled from the
    /// agent-loop event stream between `agent_start` and `agent_end`
    /// (`crate::diagnostics::RunDiagnostics`).
    ///
    /// `None` before the first run finishes. A run that recorded no turn
    /// stores nothing, so the previous summary survives a no-op run.
    pub fn last_run_diagnostics(&self) -> Option<crate::diagnostics::RunDiagnostics> {
        self.last_run_diagnostics
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Store one finished run's summary (the prompt-run wrapper's seam).
    pub(crate) fn set_last_run_diagnostics(&self, summary: crate::diagnostics::RunDiagnostics) {
        *self
            .last_run_diagnostics
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(summary);
    }
}
