//! Commit-tracker FIFO regressions (moved out of `src/agent/commit.rs`:
//! production sources may not carry inline test modules).
//!
//! A long session commits one placeholder per tool call. Committed keys must
//! leave the eviction FIFO (it tracks PENDING keys only), and releasing a gate
//! (an aborted/settled background call) must prune its queue entry too.

use pir::agent::commit::CommitTracker;

#[test]
fn committed_keys_do_not_accumulate_in_the_eviction_queue() {
    let tracker = CommitTracker::default();
    for index in 0..512 {
        let key = format!("call-{index}");
        let gate = tracker.register_for_test(vec![key.clone()]);
        tracker.commit_key_for_test(&key);
        assert!(gate.is_committed());
    }
    let (pending, order) = tracker.pending_counts_for_test();
    assert_eq!(pending, 0, "every gate was released");
    assert_eq!(order, 0, "pending_order retained committed keys");
}

#[test]
fn released_gates_do_not_accumulate_in_the_eviction_queue() {
    let tracker = CommitTracker::default();
    for index in 0..64 {
        let key = format!("call-{index}");
        let gate = tracker.register_for_test(vec![key.clone()]);
        tracker.release_for_test(&gate);
    }
    let (pending, order) = tracker.pending_counts_for_test();
    assert_eq!(pending, 0, "every gate was released");
    assert_eq!(order, 0);
}
