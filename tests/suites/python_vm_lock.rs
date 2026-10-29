//! `python_vm`'s process-wide execution lock hands a queued job the lock by
//! condition-variable notification, not by a fixed 20 ms poll.
//!
//! The probe in `src/plugins/builtin/python_vm.rs` runs the production
//! acquisition path on a second thread while the test holds the lock. It
//! reports how that waiter was woken, which is the property a regression to
//! `thread::sleep(20ms)` cannot satisfy: the waiter would be woken by its own
//! timer (`timed_out_wakes`) and the release notification would wake nobody
//! (`notified_wakes == 0`).

use std::sync::Mutex;
use std::time::Duration;

use pir::plugins::builtin::PythonTool;

/// Both probes contend for the process-wide execution lock; running them in
/// parallel makes one probe observe the other's release notification. The
/// handoff property is per-probe, so serialize the probes themselves.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn python_exec_lock_handoff_is_notification_driven() {
    let _probe = PROBE_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // Well inside the waiter's 20 ms check slice, so the only thing that can
    // wake it is the release notification.
    let hold = Duration::from_millis(2);
    let (acquired, parks, notified_wakes, timed_out_wakes, waited_micros) =
        PythonTool::exec_lock_wait_probe(hold);

    assert!(
        acquired,
        "the queued job must acquire the lock after the holder releases"
    );
    assert!(
        parks >= 1,
        "the waiter must have parked while the lock was held"
    );
    assert_eq!(
        notified_wakes, parks,
        "every park must end on the release notification, not on the slice timer \
         (parks={parks}, notified={notified_wakes}, timed_out={timed_out_wakes})"
    );
    assert_eq!(
        timed_out_wakes, 0,
        "a 2 ms hold is far inside the 20 ms check slice: no wake may be a timed-out one"
    );
    assert!(
        waited_micros < 250_000,
        "the handoff took {waited_micros}us; a 2 ms hold cannot stretch into a poll-duration wait"
    );
}

#[test]
fn python_exec_lock_probe_acquires_without_contention_pathologies() {
    let _probe = PROBE_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // Back-to-back handoffs: the lock must be free again after the first
    // probe (the guard's drop releases it before notifying), so a second run
    // sees the same one-park handoff rather than a leak.
    for _ in 0..3 {
        let (acquired, parks, notified_wakes, timed_out_wakes, _) =
            PythonTool::exec_lock_wait_probe(Duration::from_millis(1));
        assert!(acquired, "each probe run must acquire the released lock");
        assert!(parks >= 1);
        assert_eq!(notified_wakes, parks);
        assert_eq!(timed_out_wakes, 0);
    }
}
