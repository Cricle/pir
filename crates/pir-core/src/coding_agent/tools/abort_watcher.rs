//! The abort-watcher task shared by the `find` and `grep` tools.
//!
//! Both tools run their scan on `spawn_blocking` and need a cooperative abort
//! flag: the detached task watches the run's [`AbortSignal`] and flips the
//! flag, and the blocking loop checks it at its checkpoints. The guard keeps
//! the watcher from outliving the call.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::ai::types::AbortSignal;

/// Decrements a tool's live-watcher counter when the watcher future is
/// dropped. The guard is moved into the spawned task, so it also covers an
/// abort before the task's first poll.
struct WatcherLiveness(&'static AtomicUsize);

impl WatcherLiveness {
    fn track(counter: &'static AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for WatcherLiveness {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Owns one call's abort-watcher task. The watcher flips the blocking scan's
/// abort flag when the signal fires. A detached `JoinHandle` dropped while
/// the tool future is cancelled would leave the task parked on
/// `signal.cancelled()` forever, leaking one idle task and a signal clone per
/// cancellation. This guard aborts the watcher on every exit path; when the
/// tool future itself is dropped it also sets the abort flag, so the detached
/// blocking scan stops at its next checkpoint instead of running to
/// completion.
///
/// `live` is the calling tool's own counter (behind
/// `live_abort_watchers_for_tests` on `FindTool`/`GrepTool`): the tools keep
/// separate counters so their cancellation tests do not observe each other's
/// watchers.
pub(super) struct AbortWatcherGuard {
    task: Option<tokio::task::JoinHandle<()>>,
    flag: Arc<AtomicBool>,
}

impl AbortWatcherGuard {
    pub(super) fn spawn(
        signal: Option<AbortSignal>,
        flag: &Arc<AtomicBool>,
        live: &'static AtomicUsize,
    ) -> Self {
        let task = signal.map(|signal| {
            let flag = Arc::clone(flag);
            let liveness = WatcherLiveness::track(live);
            tokio::spawn(async move {
                let _liveness = liveness;
                signal.cancelled().await;
                flag.store(true, Ordering::SeqCst);
            })
        });
        Self {
            task,
            flag: Arc::clone(flag),
        }
    }

    /// The blocking scan finished normally: stop watching without touching
    /// the flag (setting it would turn a completed search into an abort
    /// error).
    pub(super) fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for AbortWatcherGuard {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            self.flag.store(true, Ordering::SeqCst);
            task.abort();
        }
    }
}
