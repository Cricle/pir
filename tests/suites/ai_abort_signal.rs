//! Regression tests for the await-able [`AbortSignal`] cancellation primitive
//! (internally a `tokio_util::sync::CancellationToken`). Proves a waiter is
//! woken immediately by `abort()` — no polling interval involved — while the
//! synchronous `is_aborted()` flag semantics stay intact.

use std::time::Duration;

use pir::ai::types::AbortSignal;

/// `cancelled()` resolves promptly (well under any former 50ms poll tick)
/// once any clone aborts the shared token.
#[tokio::test]
async fn cancelled_wakes_immediately_on_abort() {
    let signal = AbortSignal::new();
    let waiter_signal = signal.clone();
    let waiter = tokio::spawn(async move {
        waiter_signal.cancelled().await;
    });
    // Let the waiter park on the token before aborting.
    tokio::time::sleep(Duration::from_millis(20)).await;
    signal.abort();
    tokio::time::timeout(Duration::from_millis(200), waiter)
        .await
        .expect("cancelled() waiter must wake immediately after abort()")
        .expect("waiter task must not panic");
}

/// A signal never aborted leaves `cancelled()` pending: the waiter only
/// resolves through the timeout path (no spurious wakeup).
#[tokio::test]
async fn cancelled_stays_pending_until_abort() {
    let signal = AbortSignal::new();
    let waiter = tokio::spawn(async move {
        signal.cancelled().await;
        true
    });
    let resolved = tokio::time::timeout(Duration::from_millis(50), waiter).await;
    assert!(
        resolved.is_err(),
        "cancelled() must not resolve before abort()"
    );
    drop(resolved);
}

/// The synchronous checkpoint API is unchanged: `is_aborted()`/`check()`
/// observe the abort on any clone (existing callers keep working).
#[tokio::test]
async fn is_aborted_flag_semantics_unchanged() {
    let signal = AbortSignal::new();
    assert!(!signal.is_aborted());
    assert!(signal.check().is_ok());
    let clone = signal.clone();
    clone.abort();
    assert!(signal.is_aborted());
    assert!(signal.check().is_err());
    // `cancelled()` also resolves immediately when already aborted.
    tokio::time::timeout(Duration::from_millis(200), signal.cancelled())
        .await
        .expect("cancelled() must resolve at once for an already-aborted signal");
}
