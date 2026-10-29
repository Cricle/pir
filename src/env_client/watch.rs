//! `RemoteWatcher`: the daemon's `NodeFileWatcher`, driven remotely — upstream
//! `packages/env/src/watch.ts`. When the connection is lost, the watcher is
//! opened again once the connection can be started again and reports
//! `overflow`: changes in between were not seen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
        });
        let (ready_sender, ready_receiver) = oneshot::channel();
        *lock(&watcher.ready) = Some(ready_sender);
        let runner = watcher.clone();
        *lock(&watcher.task) = Some(tokio::spawn(async move { runner.run().await }));
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

    /// The task's life: one attempt, reconnects while the connection is lost.
    async fn run(self: Arc<Self>) {
        let mut first = true;
        let mut delay = RECONNECT_FIRST;
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            let ready = Arc::new(AtomicBool::new(false));
            let result = self.attempt(&ready).await;
            let was_first = first;
            first = false;
            match result {
                // The request finished (cancelled by `close()`, or the daemon
                // ended the watch).
                Ok(()) => return,
                Err(error) => {
                    if !ready.load(Ordering::SeqCst) {
                        if was_first {
                            // The opener sees it; there is no watcher to report to.
                            self.signal_ready(Err(error));
                            return;
                        }
                        if is_connection_lost(&error) {
                            if !self.backoff(&mut delay).await {
                                return;
                            }
                            continue;
                        }
                        self.fail(error);
                        return;
                    }
                    if self.closed.load(Ordering::SeqCst) {
                        return;
                    }
                    if is_connection_lost(&error) {
                        if !self.backoff(&mut delay).await {
                            return;
                        }
                        continue;
                    }
                    self.fail(error);
                    return;
                }
            }
        }
    }

    /// One `watch` request; returns when it settles or is lost.
    async fn attempt(self: &Arc<Self>, ready: &Arc<AtomicBool>) -> Result<(), RemoteError> {
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
            let this = self.clone();
            let ready_flag = ready.clone();
            Arc::new(move |event: &Value, _payload: &[u8]| {
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
        self.connection
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

    /// Wait before reconnecting; `false` when the watcher was closed meanwhile.
    async fn backoff(self: &Arc<Self>, delay: &mut Duration) -> bool {
        let (wake_sender, wake_receiver) = oneshot::channel();
        *lock(&self.wake) = Some(wake_sender);
        let slept = tokio::select! {
            _ = tokio::time::sleep(*delay) => true,
            _ = wake_receiver => false,
        };
        *lock(&self.wake) = None;
        if !slept || self.closed.load(Ordering::SeqCst) {
            return false;
        }
        *delay = (*delay * 2).min(RECONNECT_MAX);
        // The next attempt's `ready` reports the overflow this gap caused.
        self.reconnecting.store(true, Ordering::SeqCst);
        true
    }
}
