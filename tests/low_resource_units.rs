//! Unit-level regressions that cannot live inline in `src/` (the source
//! hygiene gate forbids `#[cfg(test)]` modules there). Each case drives a
//! `#[doc(hidden)]` seam that exposes exactly the behavior under test.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pir::agent_session::{
    wait_for_claim_change_for_tests, SESSION_IDLE_STATE_FOR_TESTS, SESSION_RUNNING_STATE_FOR_TESTS,
};

/// The claim wait must be woken by the run claim's release notification, not
/// by a timer poll: a silent claim change (a lost notification) leaves it
/// parked, while the former 25 ms poll loop returned within one tick.
#[tokio::test]
async fn claim_wait_is_event_driven_not_a_timer_poll() {
    let state = Arc::new(AtomicU8::new(SESSION_RUNNING_STATE_FOR_TESTS));
    let notify = Arc::new(tokio::sync::Notify::new());
    let waiter = tokio::spawn({
        let state = Arc::clone(&state);
        let notify = Arc::clone(&notify);
        async move { wait_for_claim_change_for_tests(&state, &notify).await }
    });
    // Let the waiter register on the notification.
    tokio::task::yield_now().await;
    // Change the claim WITHOUT notifying: an event-driven wait must still be
    // parked (a 25 ms poll would observe the change and return).
    state.store(SESSION_IDLE_STATE_FOR_TESTS, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !waiter.is_finished(),
        "the claim wait must be event-driven, not a 25 ms poll"
    );
    // The real release path notifies, and the wait ends promptly.
    notify.notify_waiters();
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("the notified wait ends")
        .expect("the wait task joins");
}

#[cfg(feature = "extensions")]
#[test]
fn park_slice_backs_off_while_idle_and_resets_on_work() {
    let (base, ceiling) = pir::extensions::runtime::park_slice_bounds_for_tests();
    // Work always restores the base slice (a live extension is never delayed
    // by the idle backoff).
    assert_eq!(
        pir::extensions::runtime::next_park_slice_for_tests(ceiling, true),
        base
    );
    // Empty drains double the fallback slice up to the ceiling and stay
    // there: an idle-but-live extension runtime wakes at most once a second
    // instead of 40x/s (the section 6.1 budget is <2 wakeups/s).
    assert_eq!(
        pir::extensions::runtime::next_park_slice_for_tests(base, false),
        base * 2
    );
    assert_eq!(
        pir::extensions::runtime::next_park_slice_for_tests(base * 2, false),
        base * 4
    );
    let mut slice = base;
    for _ in 0..10 {
        slice = pir::extensions::runtime::next_park_slice_for_tests(slice, false);
    }
    assert_eq!(slice, ceiling);
    assert_eq!(
        pir::extensions::runtime::next_park_slice_for_tests(ceiling, false),
        ceiling
    );
}

/// §6.1 budgets an idle host at <2 wakeups/s. The extension drive loop's
/// lost-wakeup fallback ceiling is the steady-state rate once a runtime is
/// idle, so it must be at least one second (the old 250 ms ceiling woke 4x/s).
#[cfg(feature = "extensions")]
#[test]
fn idle_extension_runtime_wakes_at_most_once_per_second() {
    let (base, ceiling) = pir::extensions::runtime::park_slice_bounds_for_tests();
    assert_eq!(base, Duration::from_millis(25));
    assert_eq!(
        ceiling,
        Duration::from_secs(1),
        "an idle extension runtime must not wake more than once a second"
    );
    assert!(
        1.0 / ceiling.as_secs_f64() < 2.0,
        "the idle wake rate must stay under the 2/s budget"
    );
}

/// Blocking host waits (`spawnPoll`, the `spawnSync` drain window) park on
/// the child's activity condvar: one park bounded by the caller's slice, not
/// a 5 ms sleep-poll loop (which cost 200 wakeups/s per waiting call).
#[cfg(feature = "extensions")]
#[test]
fn blocking_host_waits_park_on_the_condvar_instead_of_polling() {
    let (parks, notified, elapsed) =
        pir::extensions::runtime::activity_park_probe_for_tests(Duration::from_millis(150));
    assert!(!notified, "nothing signalled this probe");
    assert!(
        elapsed >= Duration::from_millis(140),
        "the wait must really sleep its slice, took {elapsed:?}"
    );
    assert_eq!(
        parks, 1,
        "one condvar park, not one wake per 5 ms of polling ({elapsed:?})"
    );
}

/// The blocking `spawnSync` post-exit drain anchors its quiet window at
/// `max(exit, last chunk) + grace`; measuring the remaining slice from the
/// last chunk alone collapses to zero when the child printed early and then
/// ran a while, turning the drain into a busy spin for the whole grace.
#[cfg(feature = "extensions")]
#[test]
fn spawn_sync_drain_slice_is_anchored_at_the_exit_not_the_last_chunk() {
    use std::time::{Duration, Instant};

    use pir::extensions::runtime::sync_drain_wait_slice_for_tests as slice;

    let now = Instant::now();
    // The last chunk landed 10 s before the exit; the exit was 100 ms ago.
    // The window ends 200 ms from now, so the slice is the 50 ms park bound
    // (the pre-fix formula computed 300 ms - 10 s = 0 and spun).
    let exit_at = now - Duration::from_millis(100);
    let last_data = now - Duration::from_secs(10);
    assert_eq!(slice(exit_at, last_data, now), Duration::from_millis(50));

    // 10 ms left of the grace: the last slice is exactly the remainder.
    let exit_at = now - Duration::from_millis(290);
    assert_eq!(
        slice(exit_at, exit_at, now),
        Duration::from_millis(10),
        "the slice never outlives the quiet deadline"
    );

    // Past the deadline: zero (the caller breaks).
    let exit_at = now - Duration::from_millis(400);
    assert!(slice(exit_at, exit_at, now).is_zero());
    assert!(
        slice(exit_at, now - Duration::from_secs(5), now).is_zero(),
        "an old last chunk must not extend the window either"
    );

    // A chunk AFTER the exit re-arms the window from the chunk.
    let exit_at = now - Duration::from_millis(400);
    let last_data = now - Duration::from_millis(100);
    assert_eq!(slice(exit_at, last_data, now), Duration::from_millis(50));
}

/// The final `spawnPoll` sample can fold two takes (the pre-drain one and the
/// tail a released backpressured reader appended); the JS side decodes each
/// payload as ONE byte chunk, so the fold must produce a single valid base64
/// payload of the concatenated bytes, not two glued strings.
#[cfg(feature = "extensions")]
#[test]
fn spawn_poll_base64_tail_folds_into_one_payload() {
    use pir::extensions::runtime::append_base64_chunk_for_tests as fold;

    // "hi" + "!" -> "hi!"
    assert_eq!(fold("aGk=", "IQ=="), "aGkh");
    // An empty tail keeps the first payload byte-for-byte (no re-encode).
    assert_eq!(fold("aGk=", ""), "aGk=");
    assert_eq!(fold("", ""), "");
}

/// The `workerRecvPoll` fallback is adaptive: an idle mailbox is re-checked
/// after 25, 50, 100, ... ms (up to 1 s), not every 25 ms, and a delivered
/// message wakes the poll immediately (its wait is really slept).
#[cfg(feature = "extensions")]
#[tokio::test]
async fn worker_recv_poll_wakes_only_on_a_message_or_its_deadline() {
    use pir::extensions::runtime::{
        next_poll_slice_for_tests, poll_slice_bounds_for_tests, test_recv_poll,
        test_register_worker, test_release_worker, test_worker_poll_wakes, test_worker_send,
    };
    use std::time::Instant;

    let (base, ceiling) = poll_slice_bounds_for_tests();
    assert_eq!(base, 25);
    assert_eq!(ceiling, 1_000);
    assert_eq!(next_poll_slice_for_tests(base, false), base * 2);
    assert_eq!(next_poll_slice_for_tests(ceiling, false), ceiling);
    assert_eq!(next_poll_slice_for_tests(ceiling, true), base);

    // An idle wait really sleeps its `waitMs`: no busy 25 ms loop returning
    // early, and O(log wait) fallback wakeups (25+50+100+125 = 4 for 300 ms).
    let idle_id = test_register_worker();
    let started = Instant::now();
    let envelope = test_recv_poll(idle_id, 300, "worker").await;
    let elapsed = started.elapsed();
    assert!(
        envelope.contains("\"messages\":[]"),
        "an empty wait reports no messages: {envelope}"
    );
    assert!(
        elapsed >= Duration::from_millis(280),
        "waitMs must be slept, returned after {elapsed:?}"
    );
    let idle_wakes = test_worker_poll_wakes(idle_id);
    assert!(
        idle_wakes <= 5,
        "the fallback must back off, not tick every 25 ms: {idle_wakes} wakeups in {elapsed:?}"
    );
    test_release_worker(idle_id);

    // A message posted mid-wait wakes the poll immediately (one fallback
    // wake at most — the first 25 ms slice, if the post lands after it).
    let live_id = test_register_worker();
    let poster = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(test_worker_send(live_id, true, r#"{"hello":1}"#));
    });
    let started = Instant::now();
    let envelope = test_recv_poll(live_id, 1_000, "worker").await;
    let elapsed = started.elapsed();
    poster.await.expect("poster task");
    assert!(
        envelope.contains("hello"),
        "the posted message must be delivered: {envelope}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "a delivered message ends the wait promptly, took {elapsed:?}"
    );
    assert!(
        test_worker_poll_wakes(live_id) <= 1,
        "a message must not cost repeated fallback wakeups"
    );
    test_release_worker(live_id);
}

#[cfg(feature = "tui")]
#[test]
fn tui_idle_wakeup_budget_is_under_two_per_second() {
    use std::time::{Duration, Instant};

    // Git-branch watcher: HEAD is polled every 5-10 s, never every second.
    let git = pir::tui::terminal::GIT_BRANCH_POLL_INTERVAL;
    assert!(
        git >= Duration::from_secs(5) && git <= Duration::from_secs(10),
        "the HEAD poll interval must be 5-10 s, got {git:?}"
    );

    // Theme watcher: with no reload armed it blocks on `recv_timeout` for the
    // stop-check interval (no fixed 25 ms tick); with a debounce armed it
    // wakes exactly at the deadline.
    let plan = pir::tui::theme::ThemeWatcherPlan::default();
    let idle = pir::tui::theme::theme_watch_timeout(&plan, Instant::now());
    assert!(
        idle >= Duration::from_millis(500),
        "an idle theme watcher must not tick faster than 2x/s, got {idle:?}"
    );

    let per_second = 1.0 / git.as_secs_f64() + 1.0 / idle.as_secs_f64();
    assert!(
        per_second < 2.0,
        "idle TUI wakeups are {per_second:.2}/s (budget: < 2/s)"
    );
}

#[cfg(feature = "rpc")]
#[test]
fn rpc_ui_poll_backs_off_when_idle() {
    use std::time::Duration;

    use pir::coding_agent::rpc::ui_poll_interval;

    let base = Duration::from_millis(50);
    let mut interval = base;
    let mut idle = Vec::new();
    for _ in 0..6 {
        interval = ui_poll_interval(interval, false);
        idle.push(interval);
    }
    assert_eq!(
        idle,
        vec![
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(400),
            Duration::from_millis(500),
            Duration::from_millis(500),
            Duration::from_millis(500),
        ],
        "an empty UI poll doubles to the 500 ms ceiling and stays there"
    );
    // Draining something snaps straight back to the 50 ms base.
    assert_eq!(ui_poll_interval(Duration::from_millis(500), true), base);
    assert_eq!(ui_poll_interval(base, true), base);
}

#[test]
fn tokio_runtime_caps_the_blocking_pool_on_small_devices() {
    use pir::env_daemon::{
        tokio_max_blocking_threads_from, tokio_worker_count_from, TOKIO_MAX_BLOCKING_ENV,
        TOKIO_WORKERS_ENV,
    };

    assert_eq!(TOKIO_WORKERS_ENV, "PIR_TOKIO_WORKERS");
    assert_eq!(TOKIO_MAX_BLOCKING_ENV, "PIR_TOKIO_MAX_BLOCKING");

    // One/two cores: never tokio's 512-thread default, never below 4.
    assert_eq!(tokio_max_blocking_threads_from(None, 1), 4);
    assert_eq!(tokio_max_blocking_threads_from(None, 2), 4);
    assert_eq!(tokio_max_blocking_threads_from(None, 3), 6);
    // Big machines still cap at 32 (512 x 2 MiB of reserved stack is the
    // default this replaces).
    assert_eq!(tokio_max_blocking_threads_from(None, 64), 32);
    // Explicit overrides win within their documented range; zero/unparseable
    // values fall back to the adaptive decision.
    assert_eq!(tokio_max_blocking_threads_from(Some("12"), 64), 12);
    assert_eq!(tokio_max_blocking_threads_from(Some("0"), 2), 4);
    assert_eq!(tokio_max_blocking_threads_from(Some("nope"), 2), 4);
    assert_eq!(tokio_max_blocking_threads_from(Some("9999"), 2), 512);

    // Worker count: unset keeps tokio's one-per-core default; the override is
    // clamped to 1..=64.
    assert_eq!(tokio_worker_count_from(None), None);
    assert_eq!(tokio_worker_count_from(Some("0")), None);
    assert_eq!(tokio_worker_count_from(Some("2")), Some(2));
    assert_eq!(tokio_worker_count_from(Some("1000")), Some(64));
}

#[test]
fn thread_stack_budget_is_consistent_across_entry_points() {
    use pir::env_daemon::{stack_budget_bytes, STACK_BUDGET_BYTES};

    // Both tiers agree today: the release tier is only lowered once the deep
    // markdown/render recursion has been measured.
    assert_eq!(stack_budget_bytes(true), STACK_BUDGET_BYTES);
    assert_eq!(stack_budget_bytes(false), STACK_BUDGET_BYTES);
    // A const assertion enforces the invariant at compile time (clippy's
    // suggested form for an assertion on constants).
    const _: () = assert!(
        STACK_BUDGET_BYTES >= 16 * 1024 * 1024,
        "the shared stack budget must stay generous for the clap/swc recursion"
    );

    // Both deeply-recursive entry points ask the shared seam; neither keeps
    // its own 64 MiB literal.
    let main = include_str!("../src/main.rs");
    let js = include_str!("../src/main_cli/js_mode.rs");
    for (name, source) in [("src/main.rs", main), ("src/main_cli/js_mode.rs", js)] {
        assert!(
            source.contains("stack_budget_bytes(cfg!(debug_assertions))"),
            "{name} must use the shared stack budget seam"
        );
        assert!(
            !source.contains("64 * 1024 * 1024"),
            "{name} must not carry its own stack-size literal"
        );
    }
}

#[cfg(feature = "grep-find")]
mod grep_budget {
    use std::path::{Path, PathBuf};

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pir-grep-byte-budget-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        dir
    }

    fn search(root: &Path, budget: usize) -> (usize, usize, bool) {
        pir::coding_agent::tools::search_budget_for_tests(root, "needle", budget).expect("search")
    }

    /// The retained match text is bounded by the caller's budget while the
    /// searcher is still walking (the grep tool passes its 8 MiB display
    /// collection budget).
    #[test]
    fn search_matches_stops_retaining_at_the_byte_budget() {
        let dir = temp_dir("single-file");
        let line = format!("needle {}\n", "x".repeat(200));
        std::fs::write(dir.join("many.txt"), line.repeat(200)).expect("fixture file");

        let budget = 4 * 1024;
        let (matches, retained, byte_limit_reached) = search(&dir, budget);

        assert!(
            byte_limit_reached,
            "the retained-text budget must stop the search"
        );
        assert!(matches > 0, "matches before the budget are kept");
        assert!(
            retained <= budget,
            "retained {retained} bytes exceeds the {budget}-byte budget"
        );
        assert!(
            matches < 200,
            "the search stopped before every match: {matches}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The budget is RUN-WIDE, not per file. Each file below holds ~1 KB of
    /// match text and the budget is 3 KB: a per-file counter would retain all
    /// ten files (~10 KB), the shared counter stops the walk after ~3 KB.
    #[test]
    fn byte_budget_is_shared_across_files_and_stops_the_walk() {
        let dir = temp_dir("multi-file");
        let line = format!("needle {}\n", "x".repeat(200));
        for index in 0..10 {
            let body = line.repeat(5);
            std::fs::write(dir.join(format!("f{index:02}.txt")), &body).expect("fixture file");
        }

        let budget = 3 * 1024;
        let (matches, retained, byte_limit_reached) = search(&dir, budget);

        assert!(byte_limit_reached, "the shared budget must stop the walk");
        assert!(
            retained <= budget,
            "retained {retained} bytes exceeds the run-wide {budget}-byte budget \
             ({matches} files were not cut off)"
        );
        assert!(
            matches < 50,
            "the walk must stop before every match (10 files x 5 lines): {matches}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A single over-budget line is rejected from its raw byte length BEFORE
    /// the line is materialized as a `String` (the old shape allocated the
    /// whole line, then compared). The match is not retained and the search
    /// stops with the byte-cap flag.
    #[test]
    fn oversized_line_is_rejected_before_materialization() {
        let dir = temp_dir("oversized-line");
        let body = format!("needle {}\n", "x".repeat(64 * 1024));
        std::fs::write(dir.join("huge.txt"), body).expect("fixture file");

        let budget = 4 * 1024;
        let (matches, _retained, byte_limit_reached) = search(&dir, budget);

        assert!(byte_limit_reached, "the oversized line trips the byte cap");
        assert_eq!(
            matches, 0,
            "the oversized line must not be retained: {matches} matches"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
