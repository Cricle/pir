//! `session::guard` — the storage failure rule and close ordering.
//!
//! Port of upstream `session.ts`'s `GuardedStorage` (`:951-1119`) and
//! `FailureLatch`. The rule under test is spec §10.1: *any* error a storage
//! method returns ends the session, with two exemptions for **reads** only —
//! a [`StorageError::Request`] (an invalid read has no durable effect) and a
//! read whose caller's context was already aborted.
//!
//! The guard takes the call as a future, so a test drives it with a scripted
//! future over a real `MemoryStorage` rather than re-implementing the whole
//! `Storage` trait. What is under test is the guard's reaction to an outcome,
//! which a future states directly.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pir::ai::types::AbortSignal;
use pir::durable::errors::{SessionFailed, StorageRequestError};
use pir::durable::session::guard::{FailureLatch, FailureLatchCore, GuardedStorage};
use pir::durable::{Context, MemoryStorage, Storage, StorageError};

fn guard() -> (GuardedStorage, Arc<FailureLatchCore>) {
    let latch = Arc::new(FailureLatchCore::new());
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    (GuardedStorage::new(storage, latch.clone()), latch)
}

fn fatal() -> StorageError {
    StorageError::Io("disk gone".to_owned())
}

fn request() -> StorageError {
    StorageError::Request(StorageRequestError::new("bad cursor"))
}

/// A context whose signal is aborted.
fn aborted_context() -> Context {
    let signal = Arc::new(AbortSignal::new());
    signal.abort();
    Context::with_shared_signal(signal)
}

// ── the failure rule ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_write_error_fails_the_session_and_the_caller_sees_it() {
    let (guard, latch) = guard();

    let error = guard
        .write(async { Err::<(), _>(fatal()) })
        .await
        .expect_err("the write fails");
    assert!(
        matches!(error, StorageError::Io(_)),
        "the caller sees its own error, not SessionFailed: {error}"
    );
    assert!(latch.is_failed(), "a write error fails the session");
}

#[tokio::test]
async fn every_later_call_rejects_with_session_failed() {
    let (guard, latch) = guard();
    guard
        .write(async { Err::<(), _>(fatal()) })
        .await
        .expect_err("the first write fails");
    assert!(latch.is_failed());

    let error = guard
        .write(async { Ok::<(), _>(()) })
        .await
        .expect_err("the session is failed");
    assert!(
        matches!(error, StorageError::SessionFailed(_)),
        "a later call sees SessionFailed: {error}"
    );
    assert!(
        error.to_string().contains("disk gone"),
        "the first cause is named: {error}"
    );
}

#[tokio::test]
async fn the_first_failure_wins() {
    let latch = FailureLatchCore::new();
    latch.fail(fatal());
    latch.fail(StorageError::Invalid("second".to_owned()));
    let first = latch.failure().expect("a failure");
    assert!(
        matches!(first, StorageError::Io(_)),
        "the first error is kept: {first}"
    );
}

#[tokio::test]
async fn a_request_error_from_a_read_is_exempt() {
    let (guard, latch) = guard();
    let context = Context::background();

    let error = guard
        .read(&context, async { Err::<(), _>(request()) })
        .await
        .expect_err("the read fails");
    assert!(matches!(error, StorageError::Request(_)));
    assert!(
        !latch.is_failed(),
        "an invalid read has no durable effect and must not fail the session"
    );

    guard
        .write(async { Ok::<(), _>(()) })
        .await
        .expect("the session still works");
}

#[tokio::test]
async fn a_request_error_from_a_write_is_not_exempt() {
    // The exemption covers reads only: a commit that answered "invalid" may
    // still have been admitted, so its outcome is unknown and the session
    // cannot continue on it.
    let (guard, latch) = guard();

    let error = guard
        .write(async { Err::<(), _>(request()) })
        .await
        .expect_err("the write fails");
    assert!(matches!(error, StorageError::Request(_)));
    assert!(
        latch.is_failed(),
        "commit and mint_id are never exempt from the failure rule"
    );
}

#[tokio::test]
async fn a_read_on_an_aborted_context_is_exempt() {
    let (guard, latch) = guard();

    let error = guard
        .read(&aborted_context(), async { Err::<(), _>(fatal()) })
        .await
        .expect_err("the read fails");
    assert!(matches!(error, StorageError::Io(_)));
    assert!(
        !latch.is_failed(),
        "a cancelled read fails only itself; the backend is not blamed"
    );
}

#[tokio::test]
async fn a_read_on_a_live_context_is_not_exempt() {
    let (guard, latch) = guard();

    guard
        .read(&Context::background(), async { Err::<(), _>(fatal()) })
        .await
        .expect_err("the read fails");
    assert!(latch.is_failed(), "an uncancelled read error is fatal");
}

#[tokio::test]
async fn a_successful_call_after_a_concurrent_failure_ends_with_session_failed() {
    // The guard re-checks health after a successful call: nothing a concurrent
    // failure touched may be used, so the success is discarded.
    let (guard, latch) = guard();
    latch.fail(fatal());

    let error = guard
        .read(&Context::background(), async { Ok::<(), _>(()) })
        .await
        .expect_err("a failed session rejects everything");
    assert!(
        matches!(error, StorageError::SessionFailed(_)),
        "the success is replaced by SessionFailed: {error}"
    );
}

#[tokio::test]
async fn a_call_that_fails_after_another_failure_reports_that_failure() {
    // This caller's own error is not what it sees: the session's first failure
    // is the one that explains the state.
    let (guard, latch) = guard();
    latch.fail(fatal());

    let error = guard
        .read(&aborted_context(), async { Err::<(), _>(request()) })
        .await
        .expect_err("the call fails");
    assert!(
        matches!(error, StorageError::SessionFailed(_)),
        "the session's failure wins over this call's error: {error}"
    );
}

// ── close ordering ──────────────────────────────────────────────────────────

#[tokio::test]
async fn close_refuses_new_calls_and_reaches_the_backend() {
    let (guard, _latch) = guard();

    guard.close().await.expect("close");
    assert!(guard.is_closing(), "close began");

    let error = guard
        .read(&Context::background(), async { Ok::<(), _>(()) })
        .await
        .expect_err("a closing session refuses reads");
    assert!(
        matches!(error, StorageError::Closed(_)),
        "a refused call is Closed, not SessionFailed: {error}"
    );
}

#[tokio::test]
async fn a_write_is_refused_once_close_began() {
    let (guard, _latch) = guard();
    guard.close().await.expect("close");

    let error = guard
        .write(async { Ok::<(), _>(()) })
        .await
        .expect_err("a closing session refuses writes");
    assert!(matches!(error, StorageError::Closed(_)));
}

#[tokio::test]
async fn close_waits_for_a_call_already_in_flight() {
    // The guard counts a call from admission to settlement, and `close` waits
    // for that count to reach zero before reaching the backend. Without the
    // wait, a paged read could start its next page against a closed backend.
    //
    // The guard is not `Clone`, so the call and the close share it through the
    // same `Arc`: the call holds one reference, `close` runs on the other.
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let guard = Arc::new(GuardedStorage::new(
        storage,
        Arc::new(FailureLatchCore::new()),
    ));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

    let call = {
        let guard = guard.clone();
        tokio::spawn(async move {
            guard
                .write(async move {
                    let _ = started_tx.send(());
                    let _ = release_rx.await;
                    Ok::<(), _>(())
                })
                .await
        })
    };
    started_rx.await.expect("the call started");
    assert_eq!(guard.in_flight(), 1, "an admitted call is counted");

    // Close begins while the call still runs. It must block on the count.
    let closer = {
        let guard = guard.clone();
        tokio::spawn(async move { guard.close().await })
    };
    tokio::task::yield_now().await;
    assert!(
        !closer.is_finished(),
        "close waits for the call in flight instead of racing it"
    );

    let _ = release_tx.send(());
    call.await.expect("join").expect("the call completes");
    closer
        .await
        .expect("join")
        .expect("close then reaches the backend");
    assert_eq!(guard.in_flight(), 0, "the call settled");
    assert!(guard.is_closing());
}

// ── the latch trait and the error shape ─────────────────────────────────────

#[tokio::test]
async fn a_custom_latch_receives_the_failure() {
    /// Counts failures instead of storing them.
    #[derive(Default)]
    struct Counting(AtomicUsize);

    impl FailureLatch for Counting {
        fn failure(&self) -> Option<StorageError> {
            None
        }

        fn fail(&self, _error: StorageError) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    let latch = Arc::new(Counting::default());
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let guard = GuardedStorage::new(storage, latch.clone());
    guard
        .write(async { Err::<(), _>(fatal()) })
        .await
        .expect_err("the write fails");
    assert_eq!(
        latch.0.load(Ordering::Acquire),
        1,
        "the latch was told exactly once"
    );
}

#[test]
fn session_failed_names_the_cause() {
    let error: StorageError = SessionFailed { cause: fatal() }.into();
    assert!(matches!(error, StorageError::SessionFailed(_)));
    assert!(
        error.to_string().contains("disk gone"),
        "the cause is reported: {error}"
    );
}
