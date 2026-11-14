//! Pin: the TUI's input wait must never sleep past work that is already
//! queued.
//!
//! The event loop's Windows wait (`wait_for_win_batch_input`) blocks on the
//! console input queue, the only *pollable* input handle. Worker events
//! (provider deltas, tool/lifecycle events) arrive on a plain bounded `mpsc`
//! channel that no poll can observe, so before the fix a delta that landed
//! mid-wait was only picked up when the poll timeout expired — measured p50
//! 9.5 ms / p95 16.7 ms of added emit→drain latency, and a matching delay on
//! the abort / tool-result repaint (see
//! `.play/lanes/p3-input-latency/REPORT.md`).
//!
//! The decision that fixes it is `batch_wait_step`: one pure step of the wait
//! (console queue depth, worker wake flag, deadline). These pins fix its
//! priority order:
//!
//! - a console record wins (crossterm's parser must never consume it);
//! - a queued worker event is a wake, not a sleep — including at/after the
//!   deadline, where sleeping would strand the event for another poll
//!   timeout;
//! - expiry with both queues empty ends the wait;
//! - neither: sleep exactly one step (never a busy loop).
//!
//! The Unix wait (`unix_input_wait_loop`) adopts the same decision with the
//! platform's own step (`unix_wait_step`): the flag is re-checked on the same
//! 1 ms step the Windows console wait uses while the loop is at the active
//! frame cadence, and an idle wait keeps ONE blocking `event::poll` (no busy
//! loop; the idle wakeup budget is unchanged). The loop is driven here with a
//! fake poll source, so its schedule is pinned on every host.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pir::tui::terminal::{
    batch_wait_step, unix_input_wait_loop, unix_wait_step, BatchWaitStep, UnixWaitWake,
    ACTIVE_POLL_TIMEOUT, IDLE_POLL_TIMEOUT,
};

#[test]
fn console_input_wins_over_everything() {
    assert_eq!(batch_wait_step(1, false, false), BatchWaitStep::Ready);
    assert_eq!(batch_wait_step(1, true, false), BatchWaitStep::Ready);
    // Input that is already queued must not be delayed by the deadline: the
    // record still has to be drained by the batch reader.
    assert_eq!(batch_wait_step(2, false, true), BatchWaitStep::Ready);
    assert_eq!(batch_wait_step(3, true, true), BatchWaitStep::Ready);
}

#[test]
fn queued_worker_event_is_a_wake() {
    assert_eq!(batch_wait_step(0, true, false), BatchWaitStep::WorkerEvent);
    // At/after the deadline a queued event is still a wake: returning
    // `Expired` here would leave the event in the channel while the loop
    // sleeps out the next poll timeout.
    assert_eq!(batch_wait_step(0, true, true), BatchWaitStep::WorkerEvent);
}

#[test]
fn idle_and_expiry_bound_the_wait() {
    // Nothing queued, deadline in the future: one sleep step (the wait's
    // sleep is bounded, so this is not a spin).
    assert_eq!(batch_wait_step(0, false, false), BatchWaitStep::Idle);
    // Nothing queued and expired: end the wait so the frame/dirty paths run.
    assert_eq!(batch_wait_step(0, false, true), BatchWaitStep::Expired);
}

/// Runs the real Unix wait loop with a fake `event::poll`: it records every
/// requested step, can set the wake flag after N sleep steps (`None` = never),
/// and can report input ready before the first step (`ready_after_steps ==
/// Some(0)`, i.e. on a probe) or during step N. The fake sleeps each requested
/// step, so the deadline arithmetic is real; the assertions are written to be
/// independent of the host's sleep granularity.
fn run_unix_wait(
    timeout: Duration,
    wake_preset: bool,
    wake_after_steps: Option<usize>,
    ready_after_steps: Option<usize>,
) -> (UnixWaitWake, Vec<Duration>) {
    let wake = AtomicBool::new(wake_preset);
    let calls = Rc::new(RefCell::new(Vec::<Duration>::new()));
    let steps = Rc::new(Cell::new(0usize));
    let outcome = {
        let calls = Rc::clone(&calls);
        let steps = Rc::clone(&steps);
        let wake_ref = &wake;
        unix_input_wait_loop(
            timeout,
            &wake,
            move |step: Duration| -> std::io::Result<bool> {
                calls.borrow_mut().push(step);
                if step.is_zero() {
                    // The loop's readiness probe: only "ready input is already
                    // queued" (`Some(0)`) wins here.
                    return Ok(ready_after_steps == Some(steps.get()));
                }
                steps.set(steps.get() + 1);
                if wake_after_steps == Some(steps.get()) {
                    wake_ref.store(true, Ordering::SeqCst);
                }
                if ready_after_steps == Some(steps.get()) {
                    return Ok(true); // an input event arrived during the step
                }
                std::thread::sleep(step); // the step really elapses
                Ok(false)
            },
        )
        .expect("the fake poll never errors")
    };
    let recorded = calls.borrow().clone();
    (outcome, recorded)
}

/// The recorded sleep steps (the loop's zero-length probes are excluded).
fn sleep_steps(calls: &[Duration]) -> Vec<Duration> {
    calls
        .iter()
        .copied()
        .filter(|step| !step.is_zero())
        .collect()
}

/// The pure step bound: at the active frame cadence the Unix wait re-checks
/// the flag on the same 1 ms step the Windows console wait uses; a longer
/// (idle) wait is one blocking poll of its whole remaining time, never a 1 ms
/// tick loop (the idle wakeup budget is one loop wakeup per
/// `IDLE_POLL_TIMEOUT`).
#[test]
fn unix_wait_step_is_the_windows_step_at_the_active_cadence_and_whole_when_idle() {
    assert_eq!(
        unix_wait_step(ACTIVE_POLL_TIMEOUT),
        Duration::from_millis(1)
    );
    assert_eq!(
        unix_wait_step(Duration::from_millis(5)),
        Duration::from_millis(1)
    );
    // The final step of an active wait never overshoots its remaining time.
    assert_eq!(
        unix_wait_step(Duration::from_millis(1)),
        Duration::from_millis(1)
    );
    assert_eq!(
        unix_wait_step(Duration::from_micros(400)),
        Duration::from_micros(400)
    );
    assert_eq!(unix_wait_step(Duration::ZERO), Duration::ZERO);
    // Idle: the whole remaining time in ONE `event::poll`.
    assert_eq!(unix_wait_step(IDLE_POLL_TIMEOUT), IDLE_POLL_TIMEOUT);
    assert_eq!(
        unix_wait_step(ACTIVE_POLL_TIMEOUT + Duration::from_millis(1)),
        ACTIVE_POLL_TIMEOUT + Duration::from_millis(1)
    );
    assert_eq!(
        unix_wait_step(Duration::from_secs(30)),
        Duration::from_secs(30)
    );
}

/// The active cadence: a worker event queued mid-wait is noticed within one
/// step — the Unix twin of the Windows wake (PIR-TUI-037's emit→drain p50
/// 1.1 ms) instead of waiting out the poll timeout.
#[test]
fn unix_wait_notices_a_worker_event_within_one_step_at_the_active_cadence() {
    let (outcome, calls) = run_unix_wait(ACTIVE_POLL_TIMEOUT, false, Some(3), None);
    assert_eq!(outcome, UnixWaitWake::WorkerEvent);
    let steps = sleep_steps(&calls);
    assert_eq!(
        steps.len(),
        3,
        "the flag is set by step 3; the wait must not run step 4"
    );
    assert!(
        steps.iter().all(|step| *step <= Duration::from_millis(1)),
        "active steps are at most 1 ms: {steps:?}"
    );
    assert_eq!(
        calls.first(),
        Some(&Duration::ZERO),
        "the readiness probe runs before the first decision"
    );
}

/// A long (idle) wait must not become a 1 ms wake loop: exactly one blocking
/// poll for the whole remaining time (the loop's own share of the <2/s idle
/// wakeup budget).
#[test]
fn unix_wait_blocks_in_one_poll_when_the_timeout_is_long() {
    let idle_shape = Duration::from_millis(60); // > ACTIVE_POLL_TIMEOUT = idle regime
    let (outcome, calls) = run_unix_wait(idle_shape, false, None, None);
    assert_eq!(outcome, UnixWaitWake::Timeout);
    // Exactly ONE step — the loop recomputes `remaining` from its deadline, so
    // the requested step is the leftover after the zero-length readiness probe
    // (a hair under `idle_shape`, never a 1 ms tick). Match on the shape, not
    // the exact instant: the host's clock always moves between those calls.
    let steps = sleep_steps(&calls);
    assert_eq!(
        steps.len(),
        1,
        "one poll of the whole remaining time: {calls:?}"
    );
    assert!(
        steps[0] <= idle_shape && steps[0] >= idle_shape - Duration::from_millis(10),
        "the single poll covers the remaining time: {steps:?}"
    );
    // One probe per decision, and the loop decides twice here (the idle step,
    // then the expired step that ends the wait) — the Windows wait re-reads
    // `batch.pending()` on the same schedule.
    assert_eq!(
        calls.iter().filter(|step| step.is_zero()).count(),
        2,
        "a probe before each decision, and one blocking poll between them: {calls:?}"
    );
}

/// The wake flag wins over the deadline, and a zero budget still takes input
/// that is already queued (the pre-fix `event::poll(ZERO)` return).
#[test]
fn unix_wait_never_sleeps_on_a_queued_worker_event() {
    // Already set with a zero budget: a wake, not a sleep (the loop re-enters
    // and drains immediately).
    let (outcome, calls) = run_unix_wait(Duration::ZERO, true, None, None);
    assert_eq!(outcome, UnixWaitWake::WorkerEvent);
    assert_eq!(
        calls,
        vec![Duration::ZERO],
        "probe, then the wake — no sleep"
    );
    // Already set with a full idle budget: same; the event is not left for
    // the idle timeout.
    let (outcome, calls) = run_unix_wait(IDLE_POLL_TIMEOUT, true, None, None);
    assert_eq!(outcome, UnixWaitWake::WorkerEvent);
    assert!(sleep_steps(&calls).is_empty());
    // Nothing queued, zero budget: no sleep and a plain timeout.
    let (outcome, calls) = run_unix_wait(Duration::ZERO, false, None, None);
    assert_eq!(outcome, UnixWaitWake::Timeout);
    assert!(sleep_steps(&calls).is_empty());
}

/// Ready input keeps its priority over a queued worker event (the same order
/// the Windows wait pins): it ends the wait whether it is already queued or
/// arrives during a step.
#[test]
fn unix_wait_returns_ready_input_before_the_worker_event() {
    // Queued before the wait (seen by the probe) while a worker event is also
    // queued: input wins.
    let (outcome, calls) = run_unix_wait(IDLE_POLL_TIMEOUT, true, None, Some(0));
    assert_eq!(outcome, UnixWaitWake::Ready);
    assert!(sleep_steps(&calls).is_empty(), "ready input must not sleep");
    // Arrives during the 2nd step: the wait ends ready without waiting out
    // the rest of the timeout.
    let (outcome, calls) = run_unix_wait(ACTIVE_POLL_TIMEOUT, false, None, Some(2));
    assert_eq!(outcome, UnixWaitWake::Ready);
    assert_eq!(sleep_steps(&calls).len(), 2);
}

/// Nothing queued: the active wait expires without ever blocking longer than
/// one step (a late wake is never hidden behind a long block).
#[test]
fn unix_wait_expires_in_bounded_steps_without_a_worker_event() {
    let (outcome, calls) = run_unix_wait(ACTIVE_POLL_TIMEOUT, false, None, None);
    assert_eq!(outcome, UnixWaitWake::Timeout);
    let steps = sleep_steps(&calls);
    assert!(
        !steps.is_empty(),
        "the active wait polls, it does not pass through"
    );
    assert!(
        steps.iter().all(|step| *step <= Duration::from_millis(1)),
        "no step may block past the next flag check: {steps:?}"
    );
}
