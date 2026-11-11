//! `HarnessOptions.on_report` vs the timer report sink, and
//! `TaskRuntime::set_timer` lifecycle, on the public `pir::durable` surface.
//!
//! Two scenarios, both real `Harness` runs over fresh JSONL roots:
//!
//! 1. **Two report sinks.** A host that declines every timer is wired with
//!    `Timers::host_timers(Some(host), timer_report)` while the harness gets
//!    `HarnessOptions.on_report`. The task calls `runtime.report(...)` and
//!    `runtime.set_timer(...)`:
//!    - the task report arrives at `on_report` as
//!      `ReportInfo::TaskError`;
//!    - the declined foreground timer arrives at the *timer* sink as the
//!      fixed `HarnessOptions.setTimer declined ...` message;
//!    - neither sink sees the other's message.
//!
//! 2. **`set_timer` lifecycle.** A host that accepts foreground timers records
//!    every arm and disarm. A phase arms one timer, then ends; the invocation
//!    end disarms it (`TimerHandle::is_cancelled` and the host disarm token),
//!    and a `set_timer` call made after the phase ended rejects with
//!    `TaskRuntimeError::Ended`. A late `report` call rejects the same way.
//!
//! Everything is offline and uses only the public `pir::durable` API. Durable
//! is core, so no cargo features are needed:
//!
//! ```text
//! cargo run --example dev_durable_reports --no-default-features
//! ```
//!
//! Pass a directory as the first argument to keep the JSONL roots there;
//! without one the example uses a fresh temp dir.

use std::error::Error;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pir::durable::errors::ReportInfo;
use pir::durable::harness::scheduler::runtime::{TaskRuntime, TaskRuntimeError};
use pir::durable::harness::scheduler::NextTaskState;
use pir::durable::harness::task::{define_task, phase_handler, RunningTask, Task};
use pir::durable::harness::{create_registry, Harness, HarnessOptions};
use pir::durable::session::transaction::{TaskOptions, TaskOwnership};
use pir::durable::timers::{
    SetTimer, TimerCallback, TimerCancel, TimerHandle, TimerOptions, Timers,
};
use pir::durable::types::{TaskOutcome, TaskState};
use pir::durable::{Context, JsonlStorage, Storage};
use serde_json::{json, Value};
use tokio::sync::oneshot;

const TASK_REPORT: &str = "dev.reports.runtime: task error via on_report (not a timer report)";
const TIMER_REPORT: &str =
    "HarnessOptions.setTimer declined a timer that paces work; using the native timer";

/// Collected fixed-message timer reports.
type TimerReports = Arc<Mutex<Vec<String>>>;
/// The `Timers::host_timers` report sink (`Arc<dyn Fn(String) + Send + Sync>`).
type ReportSink = Arc<dyn Fn(String) + Send + Sync>;
/// Collected `(message, ReportInfo)` pairs from `HarnessOptions.on_report`.
type OnReports = Arc<Mutex<Vec<(String, ReportInfo)>>>;
/// The `HarnessOptions.on_report` sink (`Arc<dyn Fn(&str, &ReportInfo) + Send + Sync>`).
type InfoSink = Arc<dyn Fn(&str, &ReportInfo) + Send + Sync>;

// ---------------------------------------------------------------------------
// Host timers
// ---------------------------------------------------------------------------

/// Declines every timer, forcing the native fallback and one timer report.
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

/// Accepts foreground timers and records arm/disarm calls.
///
/// It deliberately never fires: the lifecycle scenario only needs to see the
/// host's cancel token run when the phase (invocation) ends.
struct RecordingHostTimer {
    armed: Arc<AtomicUsize>,
    disarmed: Arc<AtomicUsize>,
}

impl SetTimer for RecordingHostTimer {
    fn set(
        &self,
        _callback: TimerCallback,
        _delay_ms: i64,
        options: TimerOptions,
    ) -> Option<TimerCancel> {
        if options.background {
            return None;
        }
        self.armed.fetch_add(1, Ordering::SeqCst);
        let disarmed = Arc::clone(&self.disarmed);
        Some(TimerCancel::new(Box::new(move || {
            disarmed.fetch_add(1, Ordering::SeqCst);
        })))
    }
}

fn report_sink() -> (TimerReports, ReportSink) {
    let messages = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&messages);
    let report: ReportSink = Arc::new(move |message: String| {
        sink.lock().unwrap().push(message);
    });
    (messages, report)
}

fn info_sink() -> (OnReports, InfoSink) {
    let reports = Arc::new(Mutex::new(Vec::<(String, ReportInfo)>::new()));
    let sink = Arc::clone(&reports);
    let report: InfoSink = Arc::new(move |message: &str, info: &ReportInfo| {
        sink.lock()
            .unwrap()
            .push((message.to_owned(), info.clone()));
    });
    (reports, report)
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

fn reports_task(handles: Arc<Mutex<Vec<Arc<TimerHandle>>>>) -> Task {
    define_task(
        "dev.reports.runtime",
        1,
        |_input| json!({ "phase": "start" }),
    )
    .phase(
        "start",
        phase_handler(
            move |_task: &RunningTask, runtime: &TaskRuntime, context: Context| {
                let runtime = runtime.clone();
                let handles = Arc::clone(&handles);
                Box::pin(async move {
                    // A non-fatal task report: this goes to HarnessOptions.on_report.
                    runtime.report(TASK_REPORT).expect("task report accepted");
                    // The timer host declines this foreground timer: the
                    // fixed decline text goes to the Timers report sink, not
                    // to on_report.
                    let timer = runtime
                        .set_timer(|| {}, 60_000)
                        .expect("declined timer falls back to the native timer");
                    handles.lock().unwrap().push(timer);
                    runtime
                        .commit(
                            move |_tx, _current| {
                                Box::pin(async move {
                                    Ok(Some(NextTaskState::Terminal {
                                        outcome: TaskOutcome::Completed {
                                            result: json!("reported"),
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
    .expect("valid dev.reports.runtime definition")
}

#[derive(Debug)]
struct LifecycleProbe {
    disarmed_at_end: bool,
    set_after_end: Option<&'static str>,
    report_after_end: Option<&'static str>,
}

fn lifecycle_task(
    handles: Arc<Mutex<Vec<Arc<TimerHandle>>>>,
    probe: Arc<Mutex<Option<oneshot::Receiver<LifecycleProbe>>>>,
) -> Task {
    define_task(
        "dev.reports.lifecycle",
        1,
        |_input| json!({ "phase": "start" }),
    )
    .phase(
        "start",
        phase_handler(
            move |_task: &RunningTask, runtime: &TaskRuntime, context: Context| {
                let runtime = runtime.clone();
                let handles = Arc::clone(&handles);
                let probe = Arc::clone(&probe);
                Box::pin(async move {
                    let handle = runtime
                        .set_timer(|| {}, 60_000)
                        .expect("host accepts the phase timer");
                    handles.lock().unwrap().push(Arc::clone(&handle));
                    let runtime_after = runtime.clone();
                    let (tx, rx) = oneshot::channel();
                    *probe.lock().unwrap() = Some(rx);
                    tokio::spawn(async move {
                        // The invocation ends when this phase returns; the
                        // scheduler drains the timer list before aborting the
                        // signal, so the timer is already cancelled here.
                        runtime_after.cancelled().await;
                        let disarmed_at_end = handle.is_cancelled();
                        let set_after_end = match runtime_after.set_timer(|| {}, 0) {
                            Err(TaskRuntimeError::Ended { .. }) => Some("Ended"),
                            Ok(_) => None,
                            Err(_) => Some("unexpected-error"),
                        };
                        let report_after_end = match runtime_after.report("late report") {
                            Err(TaskRuntimeError::Ended { .. }) => Some("Ended"),
                            Ok(()) => None,
                            Err(_) => Some("unexpected-error"),
                        };
                        let _ = tx.send(LifecycleProbe {
                            disarmed_at_end,
                            set_after_end,
                            report_after_end,
                        });
                    });
                    runtime
                        .commit(
                            move |_tx, _current| {
                                Box::pin(async move {
                                    Ok(Some(NextTaskState::Terminal {
                                        outcome: TaskOutcome::Completed {
                                            result: json!("lifecycle"),
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
    .expect("valid dev.reports.lifecycle definition")
}

/// Create one task on the root conversation and wait for its terminal state.
async fn run_one_task(harness: &Harness, kind: &str) -> Result<Value, Box<dyn Error>> {
    let conversation = harness.root(Context::background()).await?;
    let catalogue = harness.scheduler().catalogue().snapshot();
    let definition = catalogue
        .task(kind)
        .ok_or_else(|| format!("{kind} missing from the catalogue"))?;
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
// Scenarios
// ---------------------------------------------------------------------------

async fn open_harness(
    root: &std::path::Path,
    task: Task,
    timers: Timers,
    on_report: Option<InfoSink>,
) -> Result<Harness, Box<dyn Error>> {
    fs::create_dir_all(root)?;
    let storage: Arc<dyn Storage> = Arc::new(JsonlStorage::open(root).await?);
    Ok(Harness::open(
        storage,
        HarnessOptions {
            registry: Some(Arc::new(create_registry())),
            tasks: vec![task],
            timers: Some(timers),
            on_report,
            ..HarnessOptions::default()
        },
        Context::background(),
    )
    .await?)
}

async fn run_report_sinks(base: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let (timer_reports, timer_report) = report_sink();
    let (infos, on_report) = info_sink();
    let handles = Arc::new(Mutex::new(Vec::<Arc<TimerHandle>>::new()));
    let timers = Timers::host_timers(Some(Arc::new(DecliningHostTimer)), timer_report);
    let harness = open_harness(
        &base.join("report-sinks"),
        reports_task(Arc::clone(&handles)),
        timers,
        Some(on_report),
    )
    .await?;
    let result = run_one_task(&harness, "dev.reports.runtime").await?;
    harness.close().await?;

    let infos = infos.lock().unwrap().clone();
    let timer_reports = timer_reports.lock().unwrap().clone();
    let handles = handles.lock().unwrap().clone();
    let task_error = infos.len() == 1
        && infos[0].0 == TASK_REPORT
        && matches!(&infos[0].1, ReportInfo::TaskError { task, .. }
            if task.kind == "dev.reports.runtime");
    let timer_once = timer_reports.len() == 1 && timer_reports[0] == TIMER_REPORT;
    let sinks_are_distinct = !timer_reports.iter().any(|m| m == TASK_REPORT)
        && !infos.iter().any(|(m, _)| m == TIMER_REPORT);
    let timer_disarmed = handles.len() == 1 && handles[0].is_cancelled();
    println!(
        "report-sinks: result={result} on_report={} timer_report={} task_error={task_error} timer_once={timer_once} distinct={sinks_are_distinct} timer_disarmed={timer_disarmed}",
        infos.len(),
        timer_reports.len()
    );
    if result != json!("reported") {
        return Err(format!("unexpected task result: {result}").into());
    }
    if !(task_error && timer_once && sinks_are_distinct && timer_disarmed) {
        return Err(format!(
            "report sink assertions failed: infos={infos:?} timer_reports={timer_reports:?}"
        )
        .into());
    }
    Ok(())
}

async fn run_timer_lifecycle(base: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let (timer_reports, timer_report) = report_sink();
    let armed = Arc::new(AtomicUsize::new(0));
    let disarmed = Arc::new(AtomicUsize::new(0));
    let host = Arc::new(RecordingHostTimer {
        armed: Arc::clone(&armed),
        disarmed: Arc::clone(&disarmed),
    });
    let handles = Arc::new(Mutex::new(Vec::<Arc<TimerHandle>>::new()));
    let probe = Arc::new(Mutex::new(None));
    let timers = Timers::host_timers(Some(host), timer_report);
    let harness = open_harness(
        &base.join("timer-lifecycle"),
        lifecycle_task(Arc::clone(&handles), Arc::clone(&probe)),
        timers,
        None,
    )
    .await?;
    let result = run_one_task(&harness, "dev.reports.lifecycle").await?;
    let rx = probe
        .lock()
        .unwrap()
        .take()
        .ok_or("lifecycle probe was not installed")?;
    let probe = tokio::time::timeout(Duration::from_secs(5), rx).await??;
    harness.close().await?;

    let handles = handles.lock().unwrap().clone();
    let timer_reports = timer_reports.lock().unwrap().clone();
    let armed = armed.load(Ordering::SeqCst);
    let disarmed = disarmed.load(Ordering::SeqCst);
    let runtime_disarmed = handles.len() == 1 && handles[0].is_cancelled();
    let lifecycle_ok = result == json!("lifecycle")
        && armed == 1
        && disarmed == 1
        && runtime_disarmed
        && probe.disarmed_at_end
        && probe.set_after_end == Some("Ended")
        && probe.report_after_end == Some("Ended")
        && timer_reports.is_empty();
    println!(
        "timer-lifecycle: result={result} hosted_armed={armed} hosted_disarmed={disarmed} runtime_disarmed={runtime_disarmed} probe={probe:?} timer_report={} lifecycle_ok={lifecycle_ok}",
        timer_reports.len()
    );
    if !lifecycle_ok {
        return Err(format!(
            "timer lifecycle assertions failed: armed={armed} disarmed={disarmed} probe={probe:?} timer_reports={timer_reports:?}"
        )
        .into());
    }
    Ok(())
}

async fn run_inner(root: Option<String>) -> Result<(), Box<dyn Error>> {
    let (base, keep) = match root {
        Some(path) => (std::path::PathBuf::from(path), true),
        None => {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            (
                std::env::temp_dir().join(format!(
                    "pir-dev-durable-reports-{}-{stamp}",
                    std::process::id()
                )),
                false,
            )
        }
    };
    fs::create_dir_all(&base)?;
    println!("root={}", base.display());
    run_report_sinks(&base).await?;
    run_timer_lifecycle(&base).await?;

    if !keep {
        let _ = fs::remove_dir_all(&base);
    }
    println!("dev_durable_reports: OK");
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
