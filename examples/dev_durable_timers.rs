//! Minimal external host-timer example for `pir::durable`.
//!
//! It shows the exact shape documented in `docs/durable-spec.md` under
//! "Host timers (`HarnessOptions.timers`)":
//!
//! 1. a `SetTimer` implementation that really arms a host thread timer and
//!    declines background requests;
//! 2. `Timers::host_timers(Some(host), report)` feeding one `Harness` that
//!    runs a task which parks on `TaskRuntime::sleep_until`;
//! 3. a host that declines every foreground timer: the harness falls back to
//!    the native timer and the report sink receives the fixed message exactly
//!    once per `Timers` value.
//!
//! Everything is offline and uses only the public `pir::durable` surface. The
//! example is core, so it needs no cargo features:
//!
//! ```text
//! cargo run --example dev_durable_timers --no-default-features
//! ```
//!
//! Pass a directory as the first argument to keep the JSONL roots there
//! (useful for evidence); without one the example uses a fresh temp dir.

use std::error::Error;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pir::durable::harness::scheduler::runtime::TaskRuntime;
use pir::durable::harness::scheduler::NextTaskState;
use pir::durable::harness::task::{define_task, phase_handler, RunningTask, Task};
use pir::durable::harness::{create_registry, Harness, HarnessOptions};
use pir::durable::session::transaction::{TaskOptions, TaskOwnership};
use pir::durable::timers::{SetTimer, TimerCallback, TimerCancel, TimerOptions, Timers};
use pir::durable::types::{TaskOutcome, TaskState};
use pir::durable::{Context, JsonlStorage, Storage};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 1. A host timer with a real thread
// ---------------------------------------------------------------------------

/// Accepts foreground timers on a bare thread and declines background ones
/// (a deployment where a pending background timer would pin memory).
struct ThreadHostTimer {
    decline_background: bool,
    armed: Arc<AtomicUsize>,
}

impl SetTimer for ThreadHostTimer {
    fn set(
        &self,
        callback: TimerCallback,
        delay_ms: i64,
        options: TimerOptions,
    ) -> Option<TimerCancel> {
        if options.background && self.decline_background {
            return None;
        }
        self.armed.fetch_add(1, Ordering::SeqCst);
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let delay = Duration::from_millis(delay_ms.max(0) as u64);
        // The harness expects its callback to run with a runtime context: the
        // scheduler wakes through `tokio::spawn`. A real host would post the
        // fire onto its event loop; this bare thread re-enters the runtime it
        // was armed from.
        let handle = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            if !flag.load(Ordering::SeqCst) {
                let _runtime = handle.enter();
                callback();
            }
        });
        // A host must tolerate a disarm after the callback already ran.
        Some(TimerCancel::new(Box::new(move || {
            cancelled.store(true, Ordering::SeqCst);
        })))
    }
}

/// Declines every timer, forcing the native fallback for foreground ones.
struct DecliningHostTimer;

impl SetTimer for DecliningHostTimer {
    fn set(
        &self,
        _callback: TimerCallback,
        _delay_ms: i64,
        _options: TimerOptions,
    ) -> Option<TimerCancel> {
        None
    }
}

/// Collected fixed-message timer reports.
type TimerReports = Arc<Mutex<Vec<String>>>;
/// The `Timers::host_timers` report sink (`Arc<dyn Fn(String) + Send + Sync>`).
type ReportSink = Arc<dyn Fn(String) + Send + Sync>;

fn report_sink() -> (TimerReports, ReportSink) {
    let messages = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&messages);
    let report: ReportSink = Arc::new(move |message: String| {
        sink.lock().unwrap().push(message);
    });
    (messages, report)
}

// ---------------------------------------------------------------------------
// 2. One task that parks on a durable sleep
// ---------------------------------------------------------------------------

fn nap_task(delay_ms: i64) -> Task {
    define_task("dev.timers.nap", 1, |_input| json!({ "phase": "start" }))
        .phase(
            "start",
            phase_handler(
                move |_task: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        let now = runtime.now().expect("harness clock");
                        runtime
                            .sleep_until(
                                now + delay_ms,
                                json!({ "phase": "after", "slept": delay_ms }),
                                context,
                            )
                            .await
                            .expect("sleep stored");
                    })
                },
            ),
        )
        .phase(
            "after",
            phase_handler(
                |_task: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        let resumed_at = runtime.now().expect("harness clock");
                        runtime
                            .commit(
                                move |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Terminal {
                                            outcome: TaskOutcome::Completed {
                                                result: json!(resumed_at),
                                            },
                                        }))
                                    })
                                },
                                context,
                            )
                            .await
                            .expect("terminal commit");
                    })
                },
            ),
        )
        .build()
        .expect("valid dev.timers.nap definition")
}

/// Create one nap task on the harness's root conversation and wait for it.
async fn run_one_nap(harness: &Harness) -> Result<Value, Box<dyn Error>> {
    let conversation = harness.root(Context::background()).await?;
    let catalogue = harness.scheduler().catalogue().snapshot();
    let definition = catalogue
        .task("dev.timers.nap")
        .ok_or("dev.timers.nap missing from the catalogue")?;
    let id = harness
        .scheduler()
        .create_task(
            &definition,
            json!({}),
            TaskOptions {
                trace: None,
                conversation_id: Some(conversation.id()),
                ownership: TaskOwnership::Conversation,
                background: false,
                abandon_on_restart: false,
                submission_ids: Vec::new(),
            },
        )
        .await?;
    harness.resume()?;
    let record = tokio::time::timeout(
        Duration::from_secs(5),
        harness.scheduler().wait_for_task(id),
    )
    .await??;
    match record.state {
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { result },
        } => Ok(result),
        other => Err(format!("task did not complete: {other:?}").into()),
    }
}

// ---------------------------------------------------------------------------
// 3. The scenario
// ---------------------------------------------------------------------------

async fn run_inner(root: Option<String>) -> Result<(), Box<dyn Error>> {
    let (base, keep) = match root {
        Some(path) => (std::path::PathBuf::from(path), true),
        None => {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            (
                std::env::temp_dir().join(format!(
                    "pir-dev-durable-timers-{}-{stamp}",
                    std::process::id()
                )),
                false,
            )
        }
    };
    fs::create_dir_all(&base)?;
    println!("root={}", base.display());

    // 3a. A host that accepts foreground timers: the harness sleep timer is
    // armed on the host thread, fires, and the task completes. Background is
    // declined and stays silent.
    let reports_a = {
        let (reports, report) = report_sink();
        let armed = Arc::new(AtomicUsize::new(0));
        let timers = Timers::host_timers(
            Some(Arc::new(ThreadHostTimer {
                decline_background: true,
                armed: Arc::clone(&armed),
            })),
            report,
        );
        let background_declined = timers
            .background(Arc::new(|| {}) as TimerCallback, 10)
            .is_none();
        let root_a = base.join("host-accepted");
        fs::create_dir_all(&root_a)?;
        let storage: Arc<dyn Storage> = Arc::new(JsonlStorage::open(&root_a).await?);
        let harness = Harness::open(
            storage,
            HarnessOptions {
                registry: Some(Arc::new(create_registry())),
                tasks: vec![nap_task(40)],
                timers: Some(timers),
                ..HarnessOptions::default()
            },
            Context::background(),
        )
        .await?;
        let result = run_one_nap(&harness).await?;
        harness.close().await?;
        let reports = reports.lock().unwrap().clone();
        println!(
            "host-timer: background_declined={background_declined} armed_host_timers={} result={result} reports={}",
            armed.load(Ordering::SeqCst),
            reports.len()
        );
        reports
    };

    // 3b. A host that declines foreground timers: the native timer resumes the
    // task and the fixed decline message is reported once per Timers value,
    // even across a second task.
    let (reports_b, results_b) = {
        let (reports, report) = report_sink();
        let timers = Timers::host_timers(Some(Arc::new(DecliningHostTimer)), report);
        let root_b = base.join("host-declined");
        fs::create_dir_all(&root_b)?;
        let storage: Arc<dyn Storage> = Arc::new(JsonlStorage::open(&root_b).await?);
        let harness = Harness::open(
            storage,
            HarnessOptions {
                registry: Some(Arc::new(create_registry())),
                tasks: vec![nap_task(30)],
                timers: Some(timers),
                ..HarnessOptions::default()
            },
            Context::background(),
        )
        .await?;
        let first = run_one_nap(&harness).await?;
        let second = run_one_nap(&harness).await?;
        harness.close().await?;
        let collected = reports.lock().unwrap().clone();
        (collected, (first, second))
    };
    let expected =
        "HarnessOptions.setTimer declined a timer that paces work; using the native timer";
    let fallback_ok = reports_b.len() == 1 && reports_b[0] == expected;
    println!(
        "fallback: results={:?} reports={:?} fallback_ok={fallback_ok}",
        results_b, reports_b
    );
    if !reports_a.is_empty() {
        return Err(format!("accepted host reported unexpectedly: {reports_a:?}").into());
    }
    if !fallback_ok {
        return Err(format!("expected exactly one decline report, got {reports_b:?}").into());
    }

    if !keep {
        let _ = fs::remove_dir_all(&base);
    }
    println!("dev_durable_timers: OK");
    Ok(())
}

fn main() -> std::process::ExitCode {
    let root = std::env::args().nth(1);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build the example tokio runtime");
    // The unoptimized durable futures are deep; keep the same 64 MiB thread
    // shape the other examples use.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || match runtime.block_on(run_inner(root)) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                std::process::ExitCode::FAILURE
            }
        })
        .expect("spawn the example thread")
        .join()
        .expect("the example thread panicked")
}
