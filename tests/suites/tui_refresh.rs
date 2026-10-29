//! On-demand TUI refresh tests: the native-status pulse + snapshot cache
//! behind `spawn_native_status_publisher`, and the working-indicator repaint
//! gate that keeps a static idle frame from redrawing.
//!
//! The status publisher used to poll `AgentSession::plugin_statuses()` every
//! 250 ms forever. These tests pin the replacement contract: a worker signal
//! wakes it immediately, N signals collapse into at most one republish, an
//! unchanged snapshot (including the slow safety-timeout re-read) publishes
//! nothing, and the spinner predicate is closed whenever no run is in flight.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pir::tui::terminal::{
    run_status_publisher, working_indicator_repaint_due, NativeStatusCache, StatusPulse,
    NATIVE_STATUS_SAFETY_INTERVAL,
};

fn snapshot(entries: &[(&str, &str)]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|(key, text)| ((*key).to_string(), (*text).to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// Snapshot cache: unchanged reads never publish
// ---------------------------------------------------------------------------

#[test]
fn status_cache_publishes_once_per_change_and_nothing_when_unchanged() {
    let mut cache = NativeStatusCache::new();
    let phone_on = snapshot(&[("phone", "📱 ● phone 0.0.0.0:8799")]);

    assert!(cache.apply(&phone_on), "the first snapshot is a change");
    assert_eq!(cache.snapshot(), phone_on.as_slice());

    assert!(
        !cache.apply(&phone_on),
        "an identical read must not publish (no repaint)"
    );
    assert!(
        !cache.apply(&phone_on),
        "repeated identical reads stay non-events"
    );

    let phone_off = snapshot(&[("phone", "📱 ○ phone stopped")]);
    assert!(cache.apply(&phone_off), "a real change publishes once");
    assert!(!cache.apply(&phone_off), "the change is not republished");
    assert_eq!(cache.snapshot(), phone_off.as_slice());

    // An emptied snapshot is a change too (the plugin host went away).
    assert!(cache.apply(&[]));
    assert!(!cache.apply(&[]));
}

// ---------------------------------------------------------------------------
// Pulse: burst coalescing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn notification_burst_coalesces_into_one_wakeup() {
    let pulse = StatusPulse::new();
    let observed = pulse.generation();
    for _ in 0..16 {
        pulse.signal();
    }

    // The whole burst is covered by one wakeup.
    tokio::time::timeout(
        Duration::from_millis(200),
        pulse.wait_for_change(observed, Duration::from_secs(30)),
    )
    .await
    .expect("a pending generation change must wake immediately");

    // No permit per signal is queued: the next wait has to run out its own
    // safety timeout. This is what keeps a burst of worker commands from
    // causing N publisher iterations.
    let current = pulse.generation();
    let started = Instant::now();
    pulse
        .wait_for_change(current, Duration::from_millis(30))
        .await;
    assert!(
        started.elapsed() >= Duration::from_millis(20),
        "the notification burst left an extra wakeup queued (waited {:?})",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Publisher loop: signal-driven changes, silent safety re-reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_signalled_change_publishes_exactly_once() {
    let pulse = StatusPulse::new();
    let source = Arc::new(Mutex::new(snapshot(&[("phone", "📱 ○ phone stopped")])));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<(String, String)>>();
    let task = {
        let pulse = Arc::clone(&pulse);
        let source = Arc::clone(&source);
        tokio::spawn(run_status_publisher(
            pulse,
            // Long safety net: any extra publish in this test must come from
            // the signal path, not the periodic re-read.
            Duration::from_secs(60),
            move || source.lock().unwrap().clone(),
            move |next| {
                let _ = tx.send(next);
            },
        ))
    };

    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the initial snapshot is published")
        .expect("the publisher channel stays open");
    assert_eq!(first, snapshot(&[("phone", "📱 ○ phone stopped")]));

    // Change the host and signal a burst: exactly one republish.
    *source.lock().unwrap() = snapshot(&[("phone", "📱 ● phone 0.0.0.0:8799")]);
    for _ in 0..8 {
        pulse.signal();
    }

    let second = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the signalled change is published")
        .expect("the publisher channel stays open");
    assert_eq!(second, snapshot(&[("phone", "📱 ● phone 0.0.0.0:8799")]));

    // The other seven signals must not produce another snapshot: the cache
    // absorbs the coalesced re-reads and the UI gets no repaint.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err(),
        "a burst of N signals must coalesce into one publish"
    );

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn safety_timeout_does_not_republish_an_unchanged_snapshot() {
    let pulse = StatusPulse::new();
    let reads = Arc::new(AtomicUsize::new(0));
    let published = Arc::new(AtomicUsize::new(0));
    let task = {
        let pulse = Arc::clone(&pulse);
        let reads = Arc::clone(&reads);
        let published = Arc::clone(&published);
        tokio::spawn(run_status_publisher(
            pulse,
            Duration::from_millis(20),
            move || {
                reads.fetch_add(1, Ordering::SeqCst);
                snapshot(&[("phone", "📱 ○ phone stopped")])
            },
            move |_next| {
                published.fetch_add(1, Ordering::SeqCst);
            },
        ))
    };

    // Several safety intervals elapse with no worker signal and no change.
    tokio::time::sleep(Duration::from_millis(120)).await;

    assert!(
        reads.load(Ordering::SeqCst) >= 3,
        "the safety net must keep re-reading the snapshot"
    );
    assert_eq!(
        published.load(Ordering::SeqCst),
        1,
        "only the initial snapshot may publish; unchanged safety re-reads are no-ops"
    );

    task.abort();
    let _ = task.await;
}

#[test]
fn safety_net_interval_is_at_least_one_second() {
    // The out-of-band fallback must be a slow safety net, not a revived poll.
    assert!(NATIVE_STATUS_SAFETY_INTERVAL >= Duration::from_secs(1));
}

// ---------------------------------------------------------------------------
// Draw loop: no periodic repaint at idle
// ---------------------------------------------------------------------------

#[test]
fn idle_state_never_requests_a_periodic_repaint() {
    let interval = Duration::from_millis(80);
    let hour = Duration::from_secs(3600);

    // No run in flight: the clock cannot dirty the frame, no matter how much
    // time passes (the static idle frame must not repaint).
    assert!(!working_indicator_repaint_due(
        false, true, false, false, hour, interval
    ));
    assert!(!working_indicator_repaint_due(
        false, false, false, false, hour, interval
    ));

    // A run in flight keeps the spinner cadence.
    assert!(working_indicator_repaint_due(
        true, true, false, false, interval, interval
    ));
    assert!(!working_indicator_repaint_due(
        true,
        true,
        false,
        false,
        interval - Duration::from_millis(1),
        interval
    ));

    // Hidden indicator / open dialog: no repaint either.
    assert!(!working_indicator_repaint_due(
        true, false, false, false, hour, interval
    ));
    assert!(!working_indicator_repaint_due(
        true, true, true, false, hour, interval
    ));
    assert!(!working_indicator_repaint_due(
        true, true, false, true, hour, interval
    ));
}
