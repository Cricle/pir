//! JavaScript execution for the `context-mode` builtin.
//!
//! User code runs on the SAME embedded QuickJS runtime and node compatibility
//! suite the JS extension system and `pir js` use (`ExtensionRuntime`: the
//! `__pir_host` bridge, `node:fs`/`node:path`/`fetch`/timers shims and the
//! module loader). Nothing here re-implements a JS standard library or spawns
//! a `node` child: one runtime is created lazily per plugin instance and kept
//! on a dedicated thread, and every `ctx_execute*` call seeds a transpiled
//! entry module and runs it.
//!
//! The sandbox contract mirrors upstream context-mode's Code Mode:
//!
//! - only what the script writes to stdout/stderr is captured; the streams are
//!   redirected in JS to bounded buffers (`MAX_OUTPUT_CHARS` per stream), so
//!   large derivations stay inside the runtime;
//! - `process.exit(n)` ends the script with code `n` instead of terminating
//!   the host process;
//! - a per-call QuickJS interrupt deadline (`arm_deadline_for`) stops a
//!   runaway synchronous loop; an [`AbortSignal`] sets the co-operative
//!   interrupt flag;
//! - the runtime is reused, so module caches, `require` resolution and the
//!   host shims are paid once, not per call.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};

use crate::ai::types::AbortSignal;
use crate::extensions::transpile::transpile_script_entry;
use crate::extensions::{ExtensionConfig, ExtensionLimits, ExtensionRuntime};

/// Public output budget per stream (bytes). Same value the Python era used and
/// the batch auto-index bound expects.
pub const MAX_OUTPUT_CHARS: usize = 1024 * 1024;

/// Default and maximum QuickJS heap, overridable with
/// `PIR_CONTEXT_MODE_MEMORY_MB` (`0` = uncapped). A sandbox that analyzes a
/// large file needs far more than the extension host's 128 MiB safety net; the
/// cap still protects the host from an accidental OOM.
const DEFAULT_MEMORY_MB: usize = 512;
const ENV_MEMORY_MB: &str = "PIR_CONTEXT_MODE_MEMORY_MB";

/// How long an abort waits for the interrupt to unwind before the worker is
/// declared hung (the JS interrupt handler normally lands in microseconds).
const ABORT_GRACE: Duration = Duration::from_secs(5);

/// One script to run.
pub struct JsJob {
    /// Model-supplied source (already checked non-empty by the tool).
    pub code: String,
    /// `javascript` or `typescript`.
    pub language: String,
    /// Session working directory: `process.cwd()`, relative module resolution
    /// and the fs shim roots.
    pub cwd: PathBuf,
    /// Extra environment entries for this call (upstream `CTX_FILE`).
    pub env: Vec<(String, String)>,
}

/// The captured result of one script.
pub struct JsOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub aborted: bool,
}

impl JsOutcome {
    pub fn is_error(&self) -> bool {
        self.timed_out || self.aborted || self.exit_code != 0
    }
}

// ---------------------------------------------------------------------------
// JS boot overlay
// ---------------------------------------------------------------------------

/// Boot overlay installed once per runtime. Unlike `pir js`'s script boot it
/// never lets `process.exit` kill the host: an exit throws a tagged error that
/// the module runner turns into an exit code.
const BOOT: &str = r#"
(() => {
  const proc = globalThis.process;
  globalThis.__pirRealStdio = true;

  class CtxExit extends Error {
    constructor(code) {
      super("process.exit(" + code + ")");
      this.__ctxExit = true;
      this.exitCode = code;
    }
  }
  globalThis.__CtxExit = CtxExit;

  const state = {
    out: [], err: [], outBytes: 0, errBytes: 0,
    cap: __PIR_CTX_CAP__, outTruncated: false, errTruncated: false,
  };
  globalThis.__ctxState = state;

  const push = (kind, chunk) => {
    if (chunk === undefined || chunk === null) return;
    const text = typeof chunk === "string"
      ? chunk
      : (typeof chunk.toString === "function" ? chunk.toString() : String(chunk));
    const bytes = text.length;
    const key = kind === "out" ? "outBytes" : "errBytes";
    const flag = kind === "out" ? "outTruncated" : "errTruncated";
    const arr = kind === "out" ? state.out : state.err;
    const room = state.cap - state[key];
    if (room <= 0) { state[flag] = true; return; }
    if (bytes > room) {
      arr.push(text.slice(0, room));
      state[key] += room;
      state[flag] = true;
      return;
    }
    arr.push(text);
    state[key] += bytes;
  };

  const write = (kind) => function (chunk, encoding, callback) {
    push(kind, chunk);
    if (typeof encoding === "function") encoding();
    else if (typeof callback === "function") callback();
    return true;
  };
  if (proc && proc.stdout) proc.stdout.write = write("out");
  if (proc && proc.stderr) proc.stderr.write = write("err");

  proc.exit = (code) => {
    let exitCode = proc.exitCode == null ? 0 : proc.exitCode;
    if (code !== undefined && code !== null) exitCode = Number(code);
    if (!Number.isFinite(exitCode)) exitCode = 0;
    exitCode = Math.trunc(exitCode);
    proc.exitCode = exitCode;
    state.exit = exitCode;
    throw new CtxExit(exitCode);
  };

  // Node's `-e`/CJS scope for the entry: `require`, `module`, `exports` and
  // the path globals. The entry itself is ESM (top-level await works), but
  // code migrated from Node scripts keeps working.
  globalThis.__ctxReset = (argv, env) => {
    state.out.length = 0;
    state.err.length = 0;
    state.outBytes = 0;
    state.errBytes = 0;
    state.outTruncated = false;
    state.errTruncated = false;
    state.exit = 0;
    proc.exitCode = 0;
    proc.argv.length = 0;
    for (const arg of argv) proc.argv.push(arg);
    proc.argv0 = proc.argv.length > 0 ? proc.argv[0] : "ctx";
    proc.execPath = proc.argv0;
    if (env) {
      for (const key of Object.keys(env)) proc.env[key] = env[key];
      if (env.FILE_PATH) globalThis.FILE_PATH = env.FILE_PATH;
      else delete globalThis.FILE_PATH;
    }
    try {
      globalThis.require = globalThis.__pirMakeRequire(__PIR_CTX_CWD__);
      globalThis.module = { exports: {} };
      globalThis.exports = globalThis.module.exports;
      globalThis.__filename = "[ctx_execute]";
      globalThis.__dirname = ".";
    } catch (_) { /* the CJS bridge is best-effort */ }
  };

  globalThis.__pirScriptRun = async (moduleName) => {
    try {
      await import(moduleName);
      return JSON.stringify({ ok: true });
    } catch (error) {
      if (error instanceof CtxExit || (error && error.__ctxExit === true)) {
        return JSON.stringify({ ok: true, exit: error.exitCode });
      }
      const message = error && error.message !== undefined ? String(error.message) : String(error);
      const rawStack = error && error.stack ? String(error.stack) : "";
      const stack = rawStack.indexOf(message) >= 0 ? rawStack : message + (rawStack ? "\n" + rawStack : "");
      return JSON.stringify({ ok: false, message, stack });
    }
  };
})();
"#;

/// Capture reader: the only JS evaluated after a script.
const CAPTURE: &str = r#"
JSON.stringify({
  stdout: globalThis.__ctxState.out.join("") + (globalThis.__ctxState.outTruncated ? "\n... [output truncated at 1 MiB] ..." : ""),
  stderr: globalThis.__ctxState.err.join("") + (globalThis.__ctxState.errTruncated ? "\n... [output truncated at 1 MiB] ..." : ""),
  exit: (typeof process.exitCode === "number" && Number.isFinite(process.exitCode)) ? Math.trunc(process.exitCode) : 0,
})
"#;

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

enum Command {
    Run {
        job: JsJob,
        timeout: Duration,
        /// Unique per-session sequence number naming this call's entry module.
        id: u64,
        reply: oneshot::Sender<Result<JsOutcome, String>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

struct Worker {
    tx: mpsc::UnboundedSender<Command>,
}

/// A lazily started, persistent JS sandbox.
pub struct JsSession {
    inner: AsyncMutex<Option<Worker>>,
    /// Serializes job submission: the worker executes one script at a time and
    /// the abort flag belongs to the job in flight.
    run_lock: AsyncMutex<()>,
    abort: Arc<AtomicBool>,
    spawned: AtomicU64,
    next_module: AtomicU64,
    hung: AtomicBool,
    alive: AtomicBool,
}

impl JsSession {
    pub fn fresh() -> Self {
        Self {
            inner: AsyncMutex::new(None),
            run_lock: AsyncMutex::new(()),
            abort: Arc::new(AtomicBool::new(false)),
            spawned: AtomicU64::new(0),
            next_module: AtomicU64::new(0),
            hung: AtomicBool::new(false),
            alive: AtomicBool::new(false),
        }
    }

    /// Whether a runtime thread currently exists (test seam for laziness).
    pub fn has_worker(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// How many runtime threads this session has spawned.
    pub fn spawn_count(&self) -> u64 {
        self.spawned.load(Ordering::Relaxed)
    }

    async fn ensure_worker(&self) -> Result<AsyncMutexGuard<'_, Option<Worker>>, String> {
        let mut guard = self.inner.lock().await;
        let restart = match guard.as_ref() {
            Some(worker) => self.hung.load(Ordering::Relaxed) || worker.tx.is_closed(),
            None => true,
        };
        if restart {
            let worker = self.start_worker().await?;
            *guard = Some(worker);
            self.hung.store(false, Ordering::Relaxed);
        }
        Ok(guard)
    }

    async fn start_worker(&self) -> Result<Worker, String> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Command>();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
        let abort = Arc::clone(&self.abort);
        abort.store(false, Ordering::SeqCst);
        self.spawned.fetch_add(1, Ordering::Relaxed);

        let spawned = std::thread::Builder::new()
            .name("pir-ctx-js".to_owned())
            .stack_size(crate::env_daemon::stack_budget_bytes(cfg!(
                debug_assertions
            )))
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!(
                            "context-mode: failed to start the JS executor: {error}"
                        )));
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&runtime, async move {
                    let limits = ExtensionLimits {
                        memory_bytes: memory_bytes(),
                        ..ExtensionLimits::default()
                    };
                    let config = ExtensionConfig {
                        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                        mode: "script".to_owned(),
                        limits: Some(limits),
                        ..Default::default()
                    };
                    let cwd = config.cwd.clone();
                    let js = match ExtensionRuntime::new(config).await {
                        Ok(js) => js,
                        Err(error) => {
                            let _ = ready_tx.send(Err(format!(
                                "context-mode: failed to start the JS runtime: {error}"
                            )));
                            return;
                        }
                    };
                    js.allow_root(cwd.clone());
                    js.allow_root(crate::coding_agent::context::agent_dir().join("npm"));
                    js.set_interrupt_flag(Arc::clone(&abort));
                    let boot = BOOT
                        .replace("__PIR_CTX_CAP__", &MAX_OUTPUT_CHARS.to_string())
                        .replace(
                            "__PIR_CTX_CWD__",
                            &serde_json::to_string(&cwd.display().to_string())
                                .unwrap_or_else(|_| "\".\"".to_owned()),
                        );
                    if let Err(error) = js.eval_boot(&boot).await {
                        let _ = ready_tx.send(Err(format!(
                            "context-mode: JS sandbox boot failed: {error}"
                        )));
                        return;
                    }
                    let _ = ready_tx.send(Ok(()));

                    while let Some(command) = rx.recv().await {
                        match command {
                            Command::Run {
                                job,
                                timeout,
                                id,
                                reply,
                            } => {
                                let outcome = run_job(&js, job, timeout, &abort, id).await;
                                abort.store(false, Ordering::SeqCst);
                                let _ = reply.send(outcome);
                            }
                            Command::Shutdown { reply } => {
                                abort.store(true, Ordering::SeqCst);
                                let _ = reply.send(());
                                break;
                            }
                        }
                    }
                });
            })
            .map_err(|error| format!("context-mode: failed to spawn the JS worker: {error}"))?;

        match ready_rx.await {
            Ok(Ok(())) => {
                // The thread is alive and owns the runtime; `spawned` already
                // counts it. Keep the join handle detached: Drop of the
                // process ends it, and `shutdown` coordinates explicitly.
                drop(spawned);
                self.alive.store(true, Ordering::SeqCst);
                Ok(Worker { tx })
            }
            Ok(Err(error)) => {
                self.alive.store(false, Ordering::SeqCst);
                Err(error)
            }
            Err(_) => {
                self.alive.store(false, Ordering::SeqCst);
                Err("context-mode: the JS worker stopped during startup".to_owned())
            }
        }
    }

    /// Run one script. `signal` aborts the script through the QuickJS
    /// interrupt flag; `timeout` bounds both execution and the event-loop
    /// drain.
    pub async fn run(
        &self,
        job: JsJob,
        timeout: Duration,
        signal: Option<AbortSignal>,
    ) -> Result<JsOutcome, String> {
        let worker = self.ensure_worker().await?;
        let tx = match worker.as_ref() {
            Some(worker) => worker.tx.clone(),
            None => return Err("context-mode: the JS worker is unavailable".to_owned()),
        };
        drop(worker);

        let _serial = self.run_lock.lock().await;
        let (reply_tx, reply_rx) = oneshot::channel();
        let id = self.next_module.fetch_add(1, Ordering::Relaxed);
        tx.send(Command::Run {
            job,
            timeout,
            id,
            reply: reply_tx,
        })
        .map_err(|_| "context-mode: the JS worker is not running".to_owned())?;

        let aborted = match signal {
            None => reply_rx
                .await
                .map_err(|_| "context-mode: the JS worker stopped".to_owned())?,
            Some(signal) => {
                let mut reply_rx = reply_rx;
                tokio::select! {
                    reply = &mut reply_rx => {
                        reply.map_err(|_| "context-mode: the JS worker stopped".to_owned())?
                    }
                    _ = signal.cancelled() => {
                        self.abort.store(true, Ordering::SeqCst);
                        match tokio::time::timeout(ABORT_GRACE, &mut reply_rx).await {
                            Ok(reply) => reply.map_err(|_| {
                                "context-mode: the JS worker stopped".to_owned()
                            })?,
                            Err(_) => {
                                // The interrupt did not land (a host call is
                                // blocking). Declare the worker unusable so
                                // the next call starts a fresh runtime.
                                self.hung.store(true, Ordering::SeqCst);
                                self.alive.store(false, Ordering::SeqCst);
                                Ok(JsOutcome {
                                    stdout: String::new(),
                                    stderr: "context-mode: execution aborted".to_owned(),
                                    exit_code: 1,
                                    timed_out: false,
                                    aborted: true,
                                })
                            }
                        }
                    }
                }
            }
        };
        aborted
    }

    /// Stop the worker (idempotent).
    pub async fn shutdown(&self) {
        let mut guard = self.inner.lock().await;
        let worker = guard.take();
        if let Some(worker) = worker {
            let (tx, rx) = oneshot::channel();
            if worker.tx.send(Command::Shutdown { reply: tx }).is_ok() {
                let _ = tokio::time::timeout(ABORT_GRACE, rx).await;
            }
        }
        self.abort.store(true, Ordering::SeqCst);
        self.alive.store(false, Ordering::SeqCst);
    }
}

fn memory_bytes() -> Option<usize> {
    let mb = std::env::var(ENV_MEMORY_MB)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(|mb| usize::try_from(mb).unwrap_or(usize::MAX))
        .unwrap_or(DEFAULT_MEMORY_MB);
    if mb == 0 {
        None
    } else {
        Some(mb.saturating_mul(1024 * 1024))
    }
}

async fn run_job(
    js: &ExtensionRuntime,
    job: JsJob,
    timeout: Duration,
    abort: &AtomicBool,
    id: u64,
) -> Result<JsOutcome, String> {
    let started = Instant::now();
    let ext = if job.language.eq_ignore_ascii_case("typescript") {
        "ts"
    } else {
        "js"
    };
    let name = format!("context-exec-{id}.{ext}");
    let source = transpile_script_entry(&name, &job.code)
        .map_err(|error| format!("context-mode: JavaScript compile error: {error}"))?;

    let reset_argv =
        serde_json::to_string(&vec![job.cwd.display().to_string(), format!("[{ext}]")])
            .unwrap_or_else(|_| "[]".to_owned());
    let env: serde_json::Map<String, JsonValue> = job
        .env
        .iter()
        .map(|(key, value)| (key.clone(), JsonValue::String(value.clone())))
        .collect();
    let reset = format!(
        "globalThis.__ctxReset({reset_argv}, {});",
        serde_json::to_string(&env).unwrap_or_else(|_| "{}".to_owned())
    );
    js.eval_boot(&reset)
        .await
        .map_err(|error| format!("context-mode: JS sandbox reset failed: {error}"))?;

    // A fresh module name per call: the module cache is append-only, and the
    // name lives in the session directory so relative imports resolve there.
    let module_path = job.cwd.join(name);
    let module_name = crate::extensions::loader::module_name_for(&module_path);
    js.seed_module_js(&module_name, &source)?;

    let deadline = js.arm_deadline_for(timeout);
    let run = js.run_script(&module_name).await;
    let elapsed = js.deadline_elapsed(deadline);
    js.disarm_deadline(deadline);

    let mut timed_out = false;
    let mut aborted = abort.load(Ordering::SeqCst);
    let mut stderr_extra = String::new();
    let mut failed = false;

    match run {
        Ok(Ok(())) => {}
        Ok(Err(stack)) => {
            failed = true;
            stderr_extra = stack;
        }
        Err(error) => {
            if elapsed || abort.load(Ordering::SeqCst) {
                timed_out = elapsed;
                aborted = abort.load(Ordering::SeqCst);
            } else {
                failed = true;
                stderr_extra = format!("{error}");
            }
            js.recover_after_interrupt().await;
        }
    }
    if abort.load(Ordering::SeqCst) {
        aborted = true;
    }

    // Event-loop drain (timers, pending fs/fetch work). Bounded by the call
    // budget so a script cannot park the worker forever.
    if !aborted {
        let remaining = timeout.saturating_sub(started.elapsed());
        if tokio::time::timeout(remaining.max(Duration::from_millis(1)), js.wait_idle())
            .await
            .is_err()
        {
            timed_out = true;
        }
    }

    let rejections = js.take_unhandled_rejections().await;
    if !rejections.is_empty() {
        failed = true;
        for reason in rejections {
            if !stderr_extra.is_empty() {
                stderr_extra.push('\n');
            }
            stderr_extra.push_str(&reason);
        }
    }

    let captured: JsonValue = js
        .eval_json(CAPTURE)
        .await
        .map_err(|error| format!("context-mode: could not read the script result: {error}"))?;
    let stdout = captured
        .get("stdout")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut stderr = captured
        .get("stderr")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_owned();
    if !stderr_extra.is_empty() {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&stderr_extra);
    }
    if timed_out {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "context-mode: execution timed out after {:.0}s",
            timeout.as_secs_f64()
        ));
    } else if aborted {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str("context-mode: execution aborted");
    }
    let mut exit_code = captured
        .get("exit")
        .and_then(JsonValue::as_i64)
        .unwrap_or(0)
        .clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    if timed_out {
        exit_code = 124;
    } else if aborted {
        exit_code = 130;
    } else if failed && exit_code == 0 {
        exit_code = 1;
    }

    Ok(JsOutcome {
        stdout,
        stderr,
        exit_code,
        timed_out,
        aborted,
    })
}
