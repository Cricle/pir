//! Regression test for the plan-only handle session-stream cell leak.
//!
//! `set_session_streaming` registers a per-runtime cell keyed by the
//! `RuntimeState` Arc address. Actor-backed handles remove that entry from the
//! actor thread's `ActorLiveness` guard; a plan-only handle (builtin-only
//! extension plan) has no actor, so an unconditional push would leak one entry
//! per runtime and a later runtime reusing the address would inherit it
//! instead of starting from the documented fresh idle default. The handle now
//! skips the push and removes any cell on drop; this test pins the registered
//! cell count and the per-handle fresh-idle behavior.

use pir::extensions::{ExtensionConfig, ExtensionHandle};

#[test]
fn plan_only_handle_does_not_register_a_session_stream_cell() {
    let baseline = ExtensionHandle::session_stream_cell_count();

    let handle = ExtensionHandle::start_plan_only(ExtensionConfig::default());
    assert!(!handle.has_js_runtime(), "the handle must be plan-only");
    assert_eq!(
        handle.session_stream_cell_state(),
        None,
        "a fresh plan-only handle has no cell (the idle default)"
    );

    handle.set_session_streaming(true);
    assert_eq!(
        ExtensionHandle::session_stream_cell_count(),
        baseline,
        "streaming a plan-only handle must not register a session-stream cell"
    );
    assert_eq!(
        handle.session_stream_cell_state(),
        None,
        "a plan-only handle stays on the fresh idle default"
    );

    handle.set_session_streaming(false);
    assert_eq!(
        handle.session_stream_cell_state(),
        None,
        "the idle transition is a no-op without a cell"
    );

    drop(handle);
    assert_eq!(
        ExtensionHandle::session_stream_cell_count(),
        baseline,
        "dropping the plan-only handle must leave no cell behind (fresh idle)"
    );
}
