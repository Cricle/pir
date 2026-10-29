//! CPU10: cancellation is awaited, not polled.
//!
//! `AbortSignal` wraps a `tokio_util::sync::CancellationToken` and exposes
//! `cancelled()`; the server prompt run and the codemode worker protocol loop
//! used to poll `is_aborted()` on a 25 ms tick. This test pins the wake-up
//! latency of the primitive those paths now await: a cancelling select must be
//! woken by the token, not at the next poll tick.
//!
//! Determinism: each round yields once so the spawned task is parked on the
//! cancellation future before `abort()` runs, then measures the join. A
//! 25 ms sleep loop would need ~25 ms per round (1.25 s for 50 rounds); the
//! cancellation wake is immediate, so the 500 ms budget leaves a wide margin
//! for scheduler noise while still failing a tick-based implementation.

use std::time::{Duration, Instant};

use pir::ai::types::AbortSignal;

#[tokio::test(flavor = "current_thread")]
async fn abort_wakes_a_pending_select_immediately() {
    const ROUNDS: usize = 50;

    let started = Instant::now();
    for round in 0..ROUNDS {
        let signal = AbortSignal::new();
        let waiter = signal.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = std::future::pending::<()>() => {}
                _ = waiter.cancelled() => {}
            }
        });
        // One scheduler turn: the task polls the select and parks on
        // `cancelled()` while the signal is still open.
        tokio::task::yield_now().await;
        signal.abort();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap_or_else(|_| panic!("round {round}: abort did not wake the select"))
            .expect("the waiter task must not panic");
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "{ROUNDS} abort wake-ups took {elapsed:?}; a 25 ms poll tick would need ~1.25 s"
    );
}
