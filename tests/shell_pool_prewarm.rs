//! The prompt-render prewarm gate is thread-scoped: nested render passes stay
//! suppressed and the guard restores the prior depth on exit, so the first run
//! loadout warms normally.
//!
//! Standalone binary (the `tests/suites/` and `g_*` harnesses belong to other
//! lanes); the module lived inline in `src/coding_agent/tools/shell_pool.rs`,
//! which the source hygiene gate forbids.

use pir::coding_agent::tools::shell_pool::{prewarm_suppressed, without_prewarm};

#[test]
fn without_prewarm_scopes_and_restores_the_gate() {
    assert!(!prewarm_suppressed());
    without_prewarm(|| {
        assert!(prewarm_suppressed());
        without_prewarm(|| assert!(prewarm_suppressed()));
        assert!(prewarm_suppressed());
    });
    assert!(!prewarm_suppressed());
}
