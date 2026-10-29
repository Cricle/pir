//! W8-C agent-core residual pins (unregistered suite; the foreman registers
//! it in `tests/g_agent.rs`).
//!
//! Four residuals from PLAN §6b item 3:
//! - (a) a background completion may only persist at a tool-batch BOUNDARY:
//!   its placeholder-commit gate opening is not enough while the batch that
//!   carries the placeholder is still open;
//! - (b) commit gates are RUNTIME-scoped: two sessions (trackers) using the
//!   same provider call id must not open each other's gates;
//! - (c) settle persistence is tracked per-ordinal: a failed append leaves
//!   exactly the unpersisted tail for the post-run batch (no duplicate when a
//!   later message committed, no loss when one did not), and an intentional
//!   omission after a gap is still skipped;
//! - (d) the settle path owns the queue transition: once the enqueue gate is
//!   closed (before the drain), a completion can no longer enqueue a stale
//!   frame for the next prompt, and one completion id delivers at most once
//!   through a bounded marker ring.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pir::agent::CommitTracker;
use pir::agent_session::{FollowUpQueueProbe, PersistLedger};

/// (a) The completion waits for BOTH signals in order: the enclosing batch's
/// placeholder result must be persisted (A1), and the batch itself must have
/// closed (B1). Committing the placeholder while the batch is open must not
/// let the completion write: the `backgroundToolResult` custom entry would
/// project as a user message between the assistant tool call and its result.
#[tokio::test]
async fn a_completion_persists_only_at_the_tool_batch_boundary() {
    let tracker = CommitTracker::new();
    let gate = Arc::new(tracker.register_for_test(vec!["call-1".to_owned()]));
    let fired = Arc::new(AtomicBool::new(false));

    // The agent loop holds this for the whole batch (assistant message write
    // through the last tool-result write).
    let batch = tracker.begin_batch_for_test().await;
    assert!(tracker.batch_is_open());

    let waiter = {
        let tracker = Arc::clone(&tracker);
        let gate = Arc::clone(&gate);
        let fired = Arc::clone(&fired);
        tokio::spawn(async move {
            // A1 gate, then B1 barrier — the exact order of
            // `BackgroundDelivery::finish`.
            gate.wait().await;
            let _boundary = tracker.wait_for_batch_boundary_for_test().await;
            fired.store(true, Ordering::SeqCst);
        })
    };

    // The placeholder has not been written yet: the completion must wait.
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(!gate.is_committed());
    assert!(!fired.load(Ordering::SeqCst));

    // The placeholder result IS persisted now, but the batch still has more
    // results to write. The completion must still not proceed.
    tracker.commit_key_for_test("call-1");
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        gate.is_committed(),
        "commit_key must open the placeholder gate"
    );
    assert!(
        !fired.load(Ordering::SeqCst),
        "a completion persisted inside an open tool batch (B1 violated)"
    );

    // The last result is written: the batch guard drops and the completion
    // acquires the boundary.
    drop(batch);
    tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("the completion must proceed once the batch closes")
        .expect("completion task panicked");
    assert!(fired.load(Ordering::SeqCst));
    assert!(!tracker.batch_is_open());
}

/// (b) Two runtimes with the same provider call id own separate gate tables.
/// Opening B's gate must never open A's (the pre-fix process-global table
/// keyed only by call id made concurrent sessions collide).
#[tokio::test]
async fn commit_gates_are_isolated_per_runtime_for_the_same_call_id() {
    let session_a = CommitTracker::new();
    let session_b = CommitTracker::new();

    let gate_a = session_a.register_for_test(vec!["call-shared".to_owned()]);
    let gate_b = session_b.register_for_test(vec!["call-shared".to_owned()]);
    assert!(!gate_a.is_committed());
    assert!(!gate_b.is_committed());

    session_b.commit_key_for_test("call-shared");
    assert!(
        gate_b.is_committed(),
        "the owning session's placeholder commit must open its own gate"
    );
    assert!(
        !gate_a.is_committed(),
        "session B's commit opened session A's gate for the same call id"
    );

    session_a.commit_key_for_test("call-shared");
    assert!(gate_a.is_committed());
}

/// A late nested background call registered under an already-committed
/// ancestor id starts open (its placeholder was persisted before it began).
#[tokio::test]
async fn a_late_registration_under_a_committed_id_starts_open() {
    let tracker = CommitTracker::new();
    tracker.commit_key_for_test("call-parent");
    let late = tracker.register_for_test(vec!["call-parent".to_owned()]);
    assert!(late.is_committed());
}

/// (c) A failed append installs the barrier: later sink calls leave their
/// ordinals unhandled, so the post-run batch re-appends exactly the
/// unpersisted tail `[failure..)` in produced order. Nothing committed is
/// re-appended (the old `AtomicUsize` count duplicated it) and nothing after
/// the failure is skipped (loss).
#[test]
fn a_failed_append_leaves_exactly_the_unpersisted_tail() {
    let ledger = PersistLedger::new_for_test();
    let ordinals: Vec<usize> = (0..4).map(|_| ledger.claim_for_test()).collect();
    assert_eq!(ordinals, vec![0, 1, 2, 3]);

    // 0 and 1 committed; 2's append failed; 3's sink call is blocked.
    ledger.handle_for_test(0);
    ledger.handle_for_test(1);
    ledger.fail_for_test(2);
    assert!(
        !ledger.blocked_for_test(2),
        "the failing ordinal itself is retried"
    );
    assert!(
        ledger.blocked_for_test(3),
        "later sink calls must not write"
    );

    for ordinal in 2..4 {
        assert!(!ledger.is_handled_for_test(ordinal));
    }
    assert_eq!(ledger.prefix_for_test(), 2);

    // What the post-run batch does: append every ordinal it does not handle.
    let plan: Vec<usize> = (0..4)
        .filter(|ordinal| !ledger.is_handled_for_test(*ordinal))
        .collect();
    assert_eq!(
        plan,
        vec![2, 3],
        "the batch must persist exactly the tail the sink never wrote"
    );
}

/// Concurrent claims with out-of-order handled marks converge to a complete
/// prefix: every ordinal is handled exactly once and the batch has nothing
/// left to write. This is the "count that races" pin — a bare counter would
/// lose one of these increments and duplicate or skip a message.
#[test]
fn concurrent_claim_and_handle_converge_to_a_complete_prefix() {
    const WORKERS: usize = 16;
    let ledger = std::sync::Arc::new(PersistLedger::new_for_test());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(WORKERS));
    let mut workers = Vec::new();
    for _ in 0..WORKERS {
        let ledger = std::sync::Arc::clone(&ledger);
        let barrier = std::sync::Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            let ordinal = ledger.claim_for_test();
            // Reverse the handle order relative to the claim order.
            std::thread::yield_now();
            ledger.handle_for_test(ordinal);
        }));
    }
    for worker in workers {
        worker.join().expect("ledger worker panicked");
    }

    assert_eq!(ledger.prefix_for_test(), WORKERS);
    for ordinal in 0..WORKERS {
        assert!(ledger.is_handled_for_test(ordinal));
    }
    let plan: Vec<usize> = (0..WORKERS)
        .filter(|ordinal| !ledger.is_handled_for_test(*ordinal))
        .collect();
    assert!(
        plan.is_empty(),
        "the post-run batch must not re-persist handled messages: {plan:?}"
    );
}

/// An intentional omission (overflow recovery) after the failure gap is still
/// skipped by its own ordinal, not by "index < count": the batch skips 2 but
/// still writes the failed 1 in order.
#[test]
fn an_intentional_omission_after_a_gap_is_skipped_by_ordinal() {
    let ledger = PersistLedger::new_for_test();
    for _ in 0..4 {
        ledger.claim_for_test();
    }
    ledger.handle_for_test(0);
    ledger.fail_for_test(1);
    ledger.handle_for_test(2);
    ledger.handle_for_test(3);

    assert!(!ledger.is_handled_for_test(1));
    assert!(ledger.is_handled_for_test(2));
    assert!(ledger.is_handled_for_test(3));
    assert_eq!(ledger.prefix_for_test(), 1);
    let plan: Vec<usize> = (0..4)
        .filter(|ordinal| !ledger.is_handled_for_test(*ordinal))
        .collect();
    assert_eq!(plan, vec![1], "only the failed message is re-appended");
}

/// The ledger is per settle iteration: `reset` clears the prefix, the handled
/// ordinals and the failure barrier (a resumed iteration starts fresh).
#[test]
fn reset_clears_the_ledger_for_the_next_settle_iteration() {
    let ledger = PersistLedger::new_for_test();
    ledger.claim_for_test();
    ledger.handle_for_test(0);
    ledger.fail_for_test(1);
    assert!(ledger.is_handled_for_test(0));
    assert!(ledger.blocked_for_test(2));

    ledger.reset_for_test();
    assert_eq!(ledger.prefix_for_test(), 0);
    assert!(!ledger.is_handled_for_test(0));
    assert!(!ledger.blocked_for_test(1));
}

/// (d) The settle transition closes the enqueue gate before draining. A
/// completion that attempts to queue after the close must be refused, so the
/// drained frame set is exactly the frames queued while the run was live; a
/// next run reopens the gate.
#[tokio::test]
async fn a_settled_run_refuses_late_completion_frames() {
    let probe = FollowUpQueueProbe::new();

    // Live run: the frame is queued.
    assert!(probe.try_queue("early-completion"));
    assert_eq!(probe.queued().await, vec!["early-completion".to_owned()]);

    // Settle: close BEFORE the drain (the order `mark_settling` + `drain_all`
    // guarantees).
    probe.close();
    assert!(
        !probe.try_queue("late-completion"),
        "a completion enqueued after the settle gate closed (it would be injected \
         on top of its persisted entry on the next prompt)"
    );

    assert_eq!(
        probe.drain_all().await,
        vec!["early-completion".to_owned()],
        "the drain must drop live-run frames and contain no late one"
    );
    assert!(probe.queued().await.is_empty());

    // An idle session does not queue either; the next prompt reopens.
    probe.mark_idle();
    assert!(!probe.try_queue("idle-completion"));
    probe.open();
    assert!(probe.try_queue("next-run-completion"));
}

/// (d) Concurrent enqueues racing the close: every frame the gate ACCEPTED
/// must be in the settle drain (it was sent before the close transition), and
/// every frame it REFUSED must be absent. A pre-fix check-then-send could
/// accept a frame, lose the race, and leave it for the next prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_during_enqueue_storm_never_leaks_a_frame_past_the_drain() {
    let probe = Arc::new(FollowUpQueueProbe::new());
    let barrier = Arc::new(tokio::sync::Barrier::new(33));
    let mut tasks = Vec::new();
    for index in 0..32 {
        let probe = Arc::clone(&probe);
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let text = format!("completion-{index}");
            let accepted = probe.try_queue(&text);
            (text, accepted)
        }));
    }
    barrier.wait().await;
    // Start the settle transition immediately after releasing the storm.
    probe.close();
    let drained = probe.drain_all().await;

    for task in tasks {
        let (text, accepted) = task.await.expect("enqueue task panicked");
        if accepted {
            assert!(
                drained.iter().any(|frame| frame == &text),
                "an accepted frame was not in the drain (leaked into the next prompt): {text}"
            );
        } else {
            assert!(
                !drained.iter().any(|frame| frame == &text),
                "a refused frame reached the drain: {text}"
            );
        }
    }
}

/// (d) One completion id persists + queues at most once (the marker that makes
/// a delivery racing the settle path idempotent), and the marker ring is
/// bounded so a long-lived session cannot grow it without limit.
#[test]
fn one_completion_delivers_once_and_the_marker_ring_is_bounded() {
    let probe = FollowUpQueueProbe::new();
    assert!(probe.mark_delivered("call-1"));
    assert!(
        !probe.mark_delivered("call-1"),
        "a duplicate delivery of the same completion id must be refused"
    );

    for index in 0..600 {
        probe.mark_delivered(&format!("call-{index}"));
    }
    assert!(
        probe.delivered_len() <= 512,
        "the delivered-id ring must stay bounded (got {})",
        probe.delivered_len()
    );
}
