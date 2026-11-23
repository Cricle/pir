//! QuickJS-backed code execution for the `python` and `context-mode` builtins.
//!
//! The old `python_vm.rs` hosted a full embedded RustPython interpreter (VM,
//! frozen stdlib, worker thread, interrupt channel). That stack was several
//! megabytes of the shipped binary and a large part of the release build time.
//! This module keeps the same call contract — a lazily started session, a
//! per-call deadline, captured stdout/stderr and an optional eval result — but
//! runs the code on the extension host's QuickJS runtime instead.
//!
//! Model code is JavaScript. The public tool names stay stable forSettings and
//! tool-selection compatibility; only the language behind them changes.

use rquickjs::prelude::Func;
use rquickjs::{Context, Runtime, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default per-call deadline (30s; a caller may raise it up to
/// [`MAX_TIMEOUT_MS`]).
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Hard ceiling for a call's deadline (120s).
pub const MAX_TIMEOUT_MS: u64 = 120_000;
/// Per-call output cap: captured stdout and stderr each retain at most this
/// many chars plus a trailing notice.
pub const MAX_OUTPUT_CHARS: usize = 1024 * 1024;
const TIMEOUT_HINT: &str =
    "the execution deadline elapsed; split the work into smaller calls or raise the timeout deliberately";

/// One completed execution as the session reports it back.
#[derive(Debug, Clone)]
pub struct ExecOutcome {
    pub stdout: String,
    pub stderr: String,
    pub error: bool,
    pub exit_code: Option<i32>,
    pub eval_repr: Option<String>,
    pub duration_ms: u128,
}

/// A value injected into a job's namespace before the code runs.
#[cfg(feature = "context-mode")]
#[derive(Debug, Clone)]
pub enum JobGlobal {
    Text(Arc<str>),
}

/// One execution request. Typed end to end: no JSON round-trip and no clones
/// of the (possibly large) injected content beyond the `Arc` bump.
#[derive(Debug, Clone)]
pub struct PythonJob {
    pub code: String,
    pub cwd: Option<String>,
    /// Evaluate a single expression and echo its string form (`eval_repr`).
    pub eval_first: bool,
    #[cfg(feature = "context-mode")]
    pub globals: Vec<(String, JobGlobal)>,
}

impl PythonJob {
    /// A plain `python`-tool job: eval-first echo, no injected globals.
    pub fn for_python_tool(code: String, cwd: Option<String>) -> Self {
        Self {
            code,
            cwd,
            eval_first: true,
            #[cfg(feature = "context-mode")]
            globals: Vec::new(),
        }
    }
}

struct Worker {
    runtime: Runtime,
    context: Context,
}

fn spawn_worker() -> Arc<Mutex<Worker>> {
    let runtime = Runtime::new().unwrap_or_else(|e| panic!("create QuickJS runtime: {e}"));
    let context = Context::full(&runtime).unwrap_or_else(|e| panic!("create QuickJS context: {e}"));
    Arc::new(Mutex::new(Worker { runtime, context }))
}

/// A lazily started QuickJS session. A persistent session reuses its context
/// between calls (globals survive); a fresh session rebuilds the context on
/// every call while sharing the underlying runtime.
pub struct PythonSession {
    worker: Mutex<Option<Arc<Mutex<Worker>>>>,
    fresh: bool,
    label: String,
    spawns: AtomicU64,
    abort: Arc<AtomicBool>,
}

impl PythonSession {
    fn with_policy(label: &str, fresh: bool) -> Self {
        Self {
            worker: Mutex::new(None),
            fresh,
            label: label.to_owned(),
            spawns: AtomicU64::new(0),
            abort: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Persistent session used by the standalone `python` tool: globals
    /// defined in one call survive into the next.
    pub fn new() -> Self {
        Self::with_policy("javascript tool", false)
    }

    /// Fresh-namespace session used by `context-mode`: each call starts with
    /// an empty context unless globals are injected explicitly.
    pub fn fresh() -> Self {
        Self::with_policy("ctx_execute", true)
    }

    pub fn has_worker(&self) -> bool {
        self.worker.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    pub fn spawn_count(&self) -> u64 {
        self.spawns.load(Ordering::Acquire)
    }

    /// Drop the context. QuickJS needs no worker teardown; dropping the
    /// runtime is enough and is bounded.
    pub(crate) async fn shutdown(&self) {
        self.abort_worker();
    }

    /// Interrupt the running job and drop the context so the next call starts
    /// from a clean state.
    pub(crate) fn abort_worker(&self) {
        self.abort.store(true, Ordering::Release);
        if let Ok(mut guard) = self.worker.lock() {
            *guard = None;
        }
    }

    /// Run `job` with the given deadline.
    pub async fn exec(&self, job: PythonJob, timeout: Duration) -> ExecOutcome {
        let started = Instant::now();
        let worker = {
            let mut guard = match self.worker.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if guard.is_none() {
                *guard = Some(spawn_worker());
                self.spawns.fetch_add(1, Ordering::AcqRel);
            }
            match guard.as_ref() {
                Some(w) => Arc::clone(w),
                None => {
                    return failed_outcome(&format!("{}: failed to start the runtime", self.label))
                }
            }
        };
        self.abort.store(false, Ordering::Release);
        let abort = Arc::clone(&self.abort);
        let abort_check = Arc::clone(&abort);
        let deadline = Instant::now() + timeout;
        let code = job.code;
        let cwd = job.cwd;
        let eval_first = job.eval_first;
        #[cfg(feature = "context-mode")]
        let globals = job.globals;
        let label = self.label.clone();
        let fresh = self.fresh;

        let task = tokio::task::spawn_blocking(move || {
            let mut worker = match worker.lock() {
                Ok(w) => w,
                Err(p) => p.into_inner(),
            };
            // Fresh namespace per call: rebuild the Context on the shared
            // runtime so globals never leak between jobs (the worker/Runtime
            // itself is reused, so spawn_count stays at 1).
            if fresh {
                worker.context = match Context::full(&worker.runtime) {
                    Ok(c) => c,
                    Err(e) => return (Err(e.to_string()), String::new(), String::new()),
                };
            }
            worker.runtime.set_interrupt_handler(Some(Box::new(move || {
                Instant::now() >= deadline || abort_check.load(Ordering::Acquire)
            })));

            let stdout = Arc::new(Mutex::new(String::new()));
            let stderr = Arc::new(Mutex::new(String::new()));
            let print_out = Arc::clone(&stdout);
            let print_err = Arc::clone(&stderr);

            let result = worker.context.with(|ctx| -> Result<Option<String>, String> {
                if let Some(dir) = cwd.as_deref() {
                    ctx.globals()
                        .set("__pirCwd", dir.to_owned())
                        .map_err(|e| e.to_string())?;
                }
                ctx.globals()
                    .set(
                        "__pirPrint",
                        Func::from(move |text: String| -> () {
                            let _ = print_out.lock().map(|mut s| s.push_str(&text));
                        }),
                    )
                    .map_err(|e| e.to_string())?;
                ctx.globals()
                    .set(
                        "__pirPrintErr",
                        Func::from(move |text: String| -> () {
                            let _ = print_err.lock().map(|mut s| s.push_str(&text));
                        }),
                    )
                    .map_err(|e| e.to_string())?;
                // Minimal Node-like console and `print` on top of the host
                // sinks. Only what the model prints enters the transcript.
                let _: Value = ctx.eval(
                    r#"
                    // Idempotent: persistent sessions re-run this prelude on
                    // every call. `var` redeclarations are legal; `const` is
                    // not, so keep everything as `var` + function expressions.
                    var __pirStringify = function(v) {
                      if (typeof v === 'string') return v;
                      try { return JSON.stringify(v); } catch (_) { return String(v); }
                    };
                    var print = function(...args) {
                      __pirPrint(args.map(__pirStringify).join(' ') + '\n');
                    };
                    var console = {
                      log: print,
                      info: print,
                      warn: (...a) => __pirPrintErr(a.map(__pirStringify).join(' ') + '\n'),
                      error: (...a) => __pirPrintErr(a.map(__pirStringify).join(' ') + '\n'),
                    };
                    "#,
                )
                .map_err(|e| e.to_string())?;

                #[cfg(feature = "context-mode")]
                for (name, value) in globals {
                    let text = match value {
                        JobGlobal::Text(t) => t.to_string(),
                    };
                    ctx.globals()
                        .set(name, text)
                        .map_err(|e| e.to_string())?;
                }

                let value: Value = ctx.eval(code).map_err(|e| {
                    // rquickjs's Display for Error::Exception is just
                    // "Exception generated by QuickJS". Pull the real JS
                    // message off the caught value so callers see the
                    // actual error (ReferenceError, thrown message, ...).
                    let caught = ctx.catch();
                    let _ = ctx.globals().set("__pirCaught", caught);
                    let msg: String = ctx
                        .eval("typeof __pirCaught === 'object' && __pirCaught !== null && 'message' in __pirCaught ? String(__pirCaught.message) : String(__pirCaught)")
                        .unwrap_or_default();
                    if msg.is_empty() || msg == "undefined" {
                        e.to_string()
                    } else {
                        msg
                    }
                })?;
                if eval_first {
                    ctx.globals()
                        .set("__pirResult", value)
                        .map_err(|e| e.to_string())?;
                    // Only echo a real value: a statement (e.g. "var x = 1")
                    // evaluates to undefined, which the tool must report as
                    // "(no output)", not the string "undefined".
                    let repr: Option<String> = ctx
                        .eval("(__pirResult === undefined || __pirResult === null) ? undefined : String(__pirResult)")
                        .map_err(|e| e.to_string())?;
                    Ok(repr)
                } else {
                    Ok(None)
                }
            });
            worker.runtime.set_interrupt_handler(None);
            (
                result,
                stdout.lock().map(|s| s.clone()).unwrap_or_default(),
                stderr.lock().map(|s| s.clone()).unwrap_or_default(),
            )
        });

        let (result, stdout, mut stderr) = match task.await {
            Ok(v) => v,
            Err(_) => (
                Err("runtime worker panicked".to_owned()),
                String::new(),
                String::new(),
            ),
        };
        let (eval_repr, error, stderr) = match result {
            Ok(repr) => (repr, false, stderr),
            Err(message) => {
                // Detect timeout by the clock, not by rquickjs's interrupt
                // exception text (which is locale/version dependent and may
                // be empty). A job that failed before the deadline is a real
                // script error, not a timeout.
                let timed_out = Instant::now() >= deadline || abort.load(Ordering::Acquire);
                if timed_out {
                    stderr.push_str(&format!("{}: execution timed out\n{}", label, TIMEOUT_HINT));
                    // Drop the context so a timed-out job cannot taint the next.
                    if let Ok(mut guard) = self.worker.lock() {
                        *guard = None;
                    }
                } else {
                    if !message.is_empty() {
                        stderr.push_str(&message);
                        stderr.push('\n');
                    }
                }
                (None, true, stderr)
            }
        };
        ExecOutcome {
            stdout: cap_output(stdout),
            stderr: cap_output(stderr),
            error,
            exit_code: None,
            eval_repr: eval_repr.map(cap_output),
            duration_ms: started.elapsed().as_millis(),
        }
    }
}

impl Default for PythonSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Cap a captured stream at [`MAX_OUTPUT_CHARS`] with a trailing notice.
pub fn cap_output(text: String) -> String {
    if text.chars().count() <= MAX_OUTPUT_CHARS {
        return text;
    }
    let mut out: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
    out.push_str(" [output truncated at 1 MiB] ...");
    out
}

/// A synthetic failed outcome for callers that cannot start a session.
pub fn failed_outcome(message: &str) -> ExecOutcome {
    ExecOutcome {
        stdout: String::new(),
        stderr: message.to_owned(),
        error: true,
        exit_code: None,
        eval_repr: None,
        duration_ms: 0,
    }
}

/// Legacy lock-wedge threshold kept for the context-mode diagnostics surface.
/// With QuickJS there is no separate VM worker lock, so the threshold is
/// informational only.
pub(crate) fn exec_lock_wedged_after() -> std::time::Duration {
    std::time::Duration::from_secs(5)
}
