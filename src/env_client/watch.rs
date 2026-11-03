//! `RemoteWatcher`: the daemon's `NodeFileWatcher`, driven remotely — upstream
//! `packages/env/src/watch.ts`. When the connection is lost, the watcher is
//! opened again once the connection can be started again and reports
//! `overflow`: changes in between were not seen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::ai::types::AbortSignal;

use super::connection::{lock, Connection, EventFn, RequestOptions};
use super::errors::{is_connection_lost, to_file_error, FileError, FileErrorCode, RemoteError};
use super::remote_env::WatchTarget;

const RECONNECT_FIRST: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// What a watcher reports.
#[derive(Debug, Clone)]
pub enum WatchChange {
    /// Paths that changed.
    Paths(Vec<String>),
    /// Changes while the connection was lost, or while the watch mode switched
    /// to polling: some changes were not seen.
    Overflow,
    /// The watch cannot continue.
    Error(FileError),
}

/// The callback a watcher delivers to (a throwing callback must not stop the
/// watch; in Rust a callback cannot throw, so this is a plain call).
pub type WatchChangeFn = Arc<dyn Fn(WatchChange) + Send + Sync>;

/// Extra fields of the `watch` request (`mode`, `pollIntervalMs`,
/// `maxDirectories`) — upstream `RemoteWatchOptions`.
#[derive(Debug, Clone, Default)]
pub struct WatchOptions {
    pub mode: Option<String>,
    pub poll_interval_ms: Option<u64>,
    pub max_directories: Option<u64>,
}

impl WatchOptions {
    fn to_json(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        if let Some(mode) = &self.mode {
            fields.insert("mode".to_string(), Value::String(mode.clone()));
        }
        if let Some(poll_interval_ms) = self.poll_interval_ms {
            fields.insert("pollIntervalMs".to_string(), json!(poll_interval_ms));
        }
        if let Some(max_directories) = self.max_directories {
            fields.insert("maxDirectories".to_string(), json!(max_directories));
        }
        fields
    }
}

/// A watcher running in the daemon (Durable's `NodeFileWatcher` next to the
/// files), re-established after a lost connection.
pub struct RemoteWatcher {
    connection: Connection,
    targets: Vec<WatchTarget>,
    on_change: WatchChangeFn,
    options: WatchOptions,
    mode: Mutex<String>,
    closed: AtomicBool,
    /// Set while a reconnect is pending: its `ready` reports `overflow`.
    reconnecting: AtomicBool,
    /// Resolves the opener once coverage is established (or failed).
    ready: Mutex<Option<oneshot::Sender<Result<(), RemoteError>>>>,
    /// The running request's abort signal; aborting it cancels the watch.
    signal: Mutex<Option<AbortSignal>>,
    /// Interrupts a reconnect backoff wait.
    wake: Mutex<Option<oneshot::Sender<()>>>,
    task: Mutex<Option<JoinHandle<()>>>,
    /// The runtime the runner was spawned on. `Drop` schedules the grace abort
    /// through this handle rather than `Handle::try_current()`: the last
    /// handle is often released from a thread without a runtime (a sync
    /// teardown path), and the abort guard that writes the protocol CANCEL
    /// lives on the runner's runtime.
    runtime: tokio::runtime::Handle,
    /// Set when the runner task exits; the drop regression test observes the
    /// task really ending (not just the last `Arc` being released).
    runner_done: Arc<AtomicBool>,
}

/// Marks a runner task as finished when its future is dropped, including when
/// the runtime cancels it (`JoinHandle::abort`).
struct RunnerFinished(Arc<AtomicBool>);

impl Drop for RunnerFinished {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Drop for RemoteWatcher {
    /// The last handle going away must stop the runner: a watcher created and
    /// then dropped without `close()` (a failed initialization, a `?` return,
    /// an upper layer going away) has nobody left to consume changes, and its
    /// reconnect loop would otherwise outlive every owner forever. The runner
    /// itself holds only a [`Weak`] reference; this drop aborts the in-flight
    /// `watch` request, wakes a backoff wait and stops the task.
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(wake) = lock(&self.wake).take() {
            let _ = wake.send(());
        }
        if let Some(signal) = lock(&self.signal).take() {
            signal.abort();
        }
        if let Some(task) = lock(&self.task).take() {
            // Aborting the task immediately would drop the request future
            // before its abort guard can write the protocol CANCEL (that
            // guard is a detached task that needs the request future's
            // `done` channel to stay alive). Give it a short grace period,
            // then abort as the backstop for a daemon that never answers.
            //
            // The grace task is scheduled on the runner's runtime, not on the
            // dropping thread's: the last handle may be released from a
            // non-runtime thread (a sync teardown path), where
            // `Handle::try_current()` fails and aborting immediately could
            // lose the CANCEL written by the abort guard on the runner side.
            let runtime = self.runtime.clone();
            runtime.spawn(async move {
                tokio::time::sleep(Duration::from_millis(250)).await;
                task.abort();
            });
        }
    }
}

impl RemoteWatcher {
    /// Watch `targets` (resolved paths); fails like `NodeFileWatcher.open` if
    /// coverage cannot be established.
    pub async fn open(
        connection: Connection,
        targets: Vec<WatchTarget>,
        on_change: WatchChangeFn,
        options: WatchOptions,
    ) -> Result<Arc<RemoteWatcher>, RemoteError> {
        let watcher = Arc::new(RemoteWatcher {
            connection,
            targets,
            on_change,
            options,
            mode: Mutex::new("native".to_string()),
            closed: AtomicBool::new(false),
            reconnecting: AtomicBool::new(false),
            ready: Mutex::new(None),
            signal: Mutex::new(None),
            wake: Mutex::new(None),
            task: Mutex::new(None),
            runtime: tokio::runtime::Handle::current(),
            runner_done: Arc::new(AtomicBool::new(false)),
        });
        let (ready_sender, ready_receiver) = oneshot::channel();
        *lock(&watcher.ready) = Some(ready_sender);
        // The runner holds a WEAK handle: the watcher owns the JoinHandle, so
        // a strong reference inside the task would be a cycle and the last
        // external handle could never stop the watch/reconnect loop.
        let runner = Arc::downgrade(&watcher);
        let runner_done = Arc::clone(&watcher.runner_done);
        *lock(&watcher.task) = Some(tokio::spawn(async move {
            let _finished = RunnerFinished(runner_done);
            RemoteWatcher::run(runner).await;
        }));
        match ready_receiver.await {
            Ok(Ok(())) => Ok(watcher),
            Ok(Err(error)) => Err(error),
            // The task ended without reporting: the connection never came up.
            Err(_) => Err(RemoteError::lost("pi-env connection lost")),
        }
    }

    /// The mode the daemon established (`native`, or `polling` when native
    /// watching failed).
    pub fn mode(&self) -> String {
        lock(&self.mode).clone()
    }

    /// Stop watching: cancel the request and wait for the daemon to finish it.
    pub async fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(wake) = lock(&self.wake).take() {
            let _ = wake.send(());
        }
        if let Some(signal) = lock(&self.signal).take() {
            // Cancels the `watch` request; the daemon answers `{}`.
            signal.abort();
        }
        let task = lock(&self.task).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    /// `#[doc(hidden)]` test seam: a flag set when the runner task exits.
    #[doc(hidden)]
    pub fn runner_done_for_tests(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.runner_done)
    }

    /// The task's life: one attempt, reconnects while the connection is lost.
    ///
    /// The loop upgrades `watcher` only to build an attempt or to handle its
    /// result; the attempt future itself owns everything it needs, so no
    /// strong handle is held across the long-lived `watch` request. When the
    /// last external handle goes away, [`Drop`] aborts the task and the next
    /// upgrade fails.
    async fn run(watcher: Weak<Self>) {
        let mut first = true;
        let mut delay = RECONNECT_FIRST;
        loop {
            let Some(this) = watcher.upgrade() else {
                return;
            };
            if this.closed.load(Ordering::SeqCst) {
                return;
            }
            let ready = Arc::new(AtomicBool::new(false));
            let attempt = this.attempt(&ready);
            drop(this);
            let result = attempt.await;
            let was_first = first;
            first = false;
            let Some(this) = watcher.upgrade() else {
                return;
            };
            match result {
                // The request finished (cancelled by `close()`, or the daemon
                // ended the watch).
                Ok(()) => return,
                Err(error) => {
                    let ready_now = ready.load(Ordering::SeqCst);
                    // The opener's first failure is reported, not retried.
                    let reconnect = is_connection_lost(&error) && (ready_now || !was_first);
                    if reconnect {
                        if this.closed.load(Ordering::SeqCst) {
                            return;
                        }
                        // No strong handle may be held across the wait: the
                        // runner must not keep the watcher alive while sleeping.
                        drop(this);
                        if !Self::backoff(&watcher, delay).await {
                            return;
                        }
                        delay = (delay * 2).min(RECONNECT_MAX);
                        continue;
                    }
                    if !ready_now && was_first {
                        // The opener sees it; there is no watcher to report to.
                        this.signal_ready(Err(error));
                        return;
                    }
                    if ready_now && this.closed.load(Ordering::SeqCst) {
                        return;
                    }
                    this.fail(error);
                    return;
                }
            }
        }
    }

    /// One `watch` request. The returned future owns the request (connection,
    /// signal and event callback); only the callback keeps a `Weak` handle, so
    /// the runner does not keep the watcher alive while the daemon watches.
    fn attempt(
        self: &Arc<Self>,
        ready: &Arc<AtomicBool>,
    ) -> impl std::future::Future<Output = Result<(), RemoteError>> + 'static {
        let targets: Vec<Value> = self
            .targets
            .iter()
            .map(|target| {
                let exclude = target.exclude.clone().unwrap_or_default();
                json!({
                    "path": target.path,
                    "recursive": target.recursive,
                    "exclude": { "hidden": exclude.hidden, "names": exclude.names },
                })
            })
            .collect();
        let mut fields = self.options.to_json();
        fields.insert("targets".to_string(), Value::Array(targets));
        let signal = AbortSignal::new();
        *lock(&self.signal) = Some(signal.clone());
        let on_event: EventFn = {
            let this = Arc::downgrade(self);
            let ready_flag = ready.clone();
            Arc::new(move |event: &Value, _payload: &[u8]| {
                // The watcher (and every consumer) may be gone already.
                let Some(this) = this.upgrade() else {
                    return;
                };
                match event["kind"].as_str() {
                    Some("ready") => {
                        ready_flag.store(true, Ordering::SeqCst);
                        *lock(&this.mode) = if event["mode"] == "polling" {
                            "polling".to_string()
                        } else {
                            "native".to_string()
                        };
                        let reconnected = this.reconnecting.swap(false, Ordering::SeqCst);
                        this.signal_ready(Ok(()));
                        if reconnected {
                            // Changes while disconnected were not seen.
                            this.deliver(WatchChange::Overflow);
                        }
                    }
                    Some("change") => {
                        if event["mode"] == "polling" {
                            *lock(&this.mode) = "polling".to_string();
                        }
                        if event["overflow"] == Value::Bool(true) {
                            this.deliver(WatchChange::Overflow);
                        } else {
                            let paths = event["paths"]
                                .as_array()
                                .map(|paths| {
                                    paths
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_string)
                                        .collect()
                                })
                                .unwrap_or_default();
                            this.deliver(WatchChange::Paths(paths));
                        }
                    }
                    Some("error") => {
                        let code = if event["code"] == "permission_denied" {
                            FileErrorCode::PermissionDenied
                        } else {
                            FileErrorCode::Invalid
                        };
                        let message = event["message"].as_str().unwrap_or("watch failed");
                        this.deliver(WatchChange::Error(FileError::new(
                            code,
                            message.to_string(),
                            None,
                        )));
                        this.closed.store(true, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }) as EventFn
        };
        let connection = self.connection.clone();
        async move {
            connection
                .request(
                    "watch",
                    Value::Object(fields),
                    RequestOptions {
                        signal: Some(signal),
                        on_event: Some(on_event),
                        on_start: None,
                        payload: Vec::new(),
                        session: None,
                    },
                )
                .await
                .map(|_| ())
        }
    }

    /// Report the outcome to the opener, once.
    fn signal_ready(&self, result: Result<(), RemoteError>) {
        if let Some(sender) = lock(&self.ready).take() {
            let _ = sender.send(result);
        }
    }

    fn fail(&self, error: RemoteError) {
        self.deliver(WatchChange::Error(to_file_error(&error, "")));
        self.closed.store(true, Ordering::SeqCst);
    }

    fn deliver(&self, change: WatchChange) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        (self.on_change)(change);
    }

    /// Wait before reconnecting; `false` when the watcher was closed or
    /// dropped meanwhile. Like the attempt, the wait holds no strong handle:
    /// `Drop` wakes it through `wake`, so a dropped watcher leaves no task
    /// sleeping out its 30 s backoff.
    async fn backoff(watcher: &Weak<Self>, delay: Duration) -> bool {
        let Some(this) = watcher.upgrade() else {
            return false;
        };
        let (wake_sender, wake_receiver) = oneshot::channel();
        *lock(&this.wake) = Some(wake_sender);
        drop(this);
        let slept = tokio::select! {
            _ = tokio::time::sleep(delay) => true,
            _ = wake_receiver => false,
        };
        let Some(this) = watcher.upgrade() else {
            return false;
        };
        *lock(&this.wake) = None;
        if !slept || this.closed.load(Ordering::SeqCst) {
            return false;
        }
        // The next attempt's `ready` reports the overflow this gap caused.
        this.reconnecting.store(true, Ordering::SeqCst);
        true
    }
}
