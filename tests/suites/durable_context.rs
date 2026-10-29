//! Durable context and cancellation.
//!
//! `Context` is upstream chord's invocation-scoped carrier (abort signal +
//! telemetry span). Tests live here as an integration suite
//! (`tests/suites/durable_context.rs`) because the crate forbids inline tests.

use std::sync::Arc;

use pir::ai::types::AbortSignal;
use pir::durable::Context;

#[test]
fn background_has_no_signal() {
    let context = Context::background();
    assert!(context.signal().is_none());
    assert!(!context.is_aborted());
    assert!(!context.is_detached());
    assert_eq!(context.to_string(), "[Context BACKGROUND_CONTEXT]");
}

#[test]
fn clones_share_the_signal() {
    let source = AbortSignal::new();
    let context = Context::with_shared_signal(Arc::new(source.clone()));
    let clone = context.clone();
    source.abort();
    assert!(context.is_aborted());
    assert!(clone.is_aborted());
}

#[test]
fn without_abort_signal_detaches_and_keeps_span() {
    let signal = AbortSignal::new();
    let context =
        Context::with_shared_signal(Arc::new(signal.clone())).with_span("pi.durable.phase");
    let detached = context.without_abort_signal();
    signal.abort();
    assert!(context.is_aborted());
    assert!(!detached.is_aborted());
    assert!(detached.is_detached());
    assert_eq!(detached.span(), Some("pi.durable.phase"));
}

#[test]
fn with_abort_signal_preserves_detached_flag() {
    let signal = AbortSignal::new();
    let detached = Context::background().without_abort_signal();
    let attached = detached.with_abort_signal(Arc::new(signal.clone()));
    assert!(attached.is_detached());
    signal.abort();
    assert!(attached.is_aborted());
}

#[tokio::test]
async fn cancelled_resolves_only_after_abort() {
    let source = AbortSignal::new();
    let context = Context::with_shared_signal(Arc::new(source.clone()));
    let waiter = context.clone();
    let handle = tokio::spawn(async move {
        waiter.cancelled().await;
        true
    });
    tokio::task::yield_now().await;
    assert!(!handle.is_finished());
    source.abort();
    assert!(handle.await.expect("join"));
}
