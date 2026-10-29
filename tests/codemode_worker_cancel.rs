//! Tests for the codemode worker-parent's in-flight tool-task cancellation
//! (P1-4) and the protocol frame-size cap (security review follow-up).
//!
//! The full `run_in_worker_process` parent loop needs a real worker child
//! process (`current_exe() __codemode-worker`), so these tests drive the
//! extracted pieces directly:
//!
//! - [`settle_tool_tasks`]: the collection/cancellation helper every parent
//!   teardown path calls after killing the worker — spawned tokio tasks
//!   standing in for executor calls (they mostly await, like the real
//!   subprocess waits).
//! - [`read_event_line`]: the parent's bounded frame reader (8 MiB cap).
//!
//! The sandbox-thread lifecycle test (E-2/E11) additionally drives
//! `pir::plugins::codemode::run_sandbox_for_tests` — the in-process thread
//! sandbox with no tools — to prove that dropping a run future aborts the JS
//! job and releases the thread instead of leaving it to run out its deadline.
//!
//! New file owned by the P1-4 fix; the `g_*.rs` / `suites/` files belong to
//! another workstream and are untouched.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use pir::plugins::codemode::worker::{
    read_event_line, settle_tool_tasks, ToolTaskSet, MAX_FRAME_LINE_BYTES,
};

/// A stand-in long-running tool task: sleeps far past the drain budget.
/// `done` is only set if the task is allowed to run to completion.
fn spawn_stuck_task(done: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        done.store(true, Ordering::Release);
    })
}

fn push_task(tasks: &ToolTaskSet, handle: tokio::task::JoinHandle<()>) {
    tasks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(handle);
}

#[tokio::test]
async fn cancel_mode_aborts_in_flight_tasks() {
    let tasks: ToolTaskSet = Arc::default();
    let done = Arc::new(AtomicBool::new(false));
    push_task(&tasks, spawn_stuck_task(Arc::clone(&done)));

    let started = std::time::Instant::now();
    settle_tool_tasks(&tasks, false).await;

    // The stuck task must not run to its 60s sleep: the up-front cancel
    // ends it at its next await point, so the drain returns almost
    // immediately and the task never completes.
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(5), "drain took {elapsed:?}");
    assert!(
        !done.load(Ordering::Acquire),
        "cancelled task ran to completion"
    );
}

#[tokio::test]
async fn graceful_mode_waits_for_tasks_to_finish() {
    let tasks: ToolTaskSet = Arc::default();
    let done = Arc::new(AtomicBool::new(false));
    let done_for_task = Arc::clone(&done);
    push_task(
        &tasks,
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            done_for_task.store(true, Ordering::Release);
        }),
    );

    let started = std::time::Instant::now();
    settle_tool_tasks(&tasks, true).await;

    assert!(
        done.load(Ordering::Acquire),
        "graceful drain did not wait for the task to finish"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "graceful drain returned before the task finished"
    );
}

#[tokio::test]
async fn graceful_mode_gives_up_and_aborts_past_the_drain_budget() {
    let tasks: ToolTaskSet = Arc::default();
    let done = Arc::new(AtomicBool::new(false));
    push_task(&tasks, spawn_stuck_task(Arc::clone(&done)));

    let started = std::time::Instant::now();
    settle_tool_tasks(&tasks, true).await;
    let elapsed = started.elapsed();

    // Bounded: the drain gives up after its (2s) budget instead of waiting
    // out the task's 60s sleep, and the straggler is cancelled so it cannot
    // keep running in the background.
    assert!(elapsed < Duration::from_secs(10), "drain took {elapsed:?}");
    assert!(
        !done.load(Ordering::Acquire),
        "abandoned straggler ran to completion"
    );
}

#[tokio::test]
async fn empty_task_set_returns_immediately() {
    let tasks: ToolTaskSet = Arc::default();
    let started = std::time::Instant::now();
    settle_tool_tasks(&tasks, false).await;
    settle_tool_tasks(&tasks, true).await;
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "draining an empty set must be a no-op"
    );
}

#[tokio::test]
async fn read_event_line_accepts_normal_frames() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let reader = tokio::io::BufReader::new(server);
    let mut lines = reader.lines();
    client
        .write_all(b"{\"type\":\"ready\",\"v\":1}\n")
        .await
        .unwrap();
    client.shutdown().await.unwrap();

    let line = read_event_line(&mut lines).await.unwrap();
    assert_eq!(
        line.as_deref(),
        Some("{\"type\":\"ready\",\"v\":1}"),
        "the frame line passes through (next_line strips the newline; decode_event trims)"
    );
    // EOF afterwards.
    let eof = read_event_line(&mut lines).await.unwrap();
    assert!(eof.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_event_line_rejects_oversized_frames() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let reader = tokio::io::BufReader::new(server);
    let mut lines = reader.lines();
    // Exactly one byte past the cap, no newline: the reader must return a
    // protocol-read failure (at EOF the buffered line completes and the cap
    // verdict lands), not truncate silently.
    let oversized = vec![b'x'; MAX_FRAME_LINE_BYTES + 1];
    let writer = tokio::spawn(async move {
        client.write_all(&oversized).await.unwrap();
        client.shutdown().await.ok();
    });

    let result = read_event_line(&mut lines).await;
    assert!(result.is_err(), "an over-cap frame line must be an error");
    writer.await.unwrap();
}

// ---------------------------------------------------------------------------
// Sandbox thread lifecycle (E-2/E11)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_sandbox_run_does_not_keep_its_thread_alive() {
    use pir::plugins::codemode::{run_sandbox_for_tests, sandbox_threads_alive};

    let baseline = sandbox_threads_alive();
    // An infinite script with a 120 s deadline: only cancellation can stop it.
    let mut run = Box::pin(run_sandbox_for_tests("while (true) {}", 120_000));

    // Drive the run future until its sandbox thread reports itself alive.
    let started = async {
        let deadline = Instant::now() + Duration::from_secs(30);
        while sandbox_threads_alive() <= baseline {
            assert!(
                Instant::now() < deadline,
                "the sandbox thread never started"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::select! {
        _ = &mut run => panic!("the infinite script finished before cancellation"),
        _ = started => {}
    }
    assert_eq!(
        sandbox_threads_alive(),
        baseline + 1,
        "the sandbox thread must be alive before the cancellation"
    );

    // Dropping the future is the cancellation: the caller is gone (a task
    // abort / a dropped tool call). The thread must abort its JS job and exit
    // instead of running to its own 120 s deadline.
    drop(run);

    let deadline = Instant::now() + Duration::from_secs(10);
    while sandbox_threads_alive() > baseline {
        assert!(
            Instant::now() < deadline,
            "a cancelled sandbox run kept its thread alive instead of \
             releasing it well before the 120 s script deadline"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
