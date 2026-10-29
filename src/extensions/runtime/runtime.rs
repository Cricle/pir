use super::*;

use std::collections::VecDeque;

/// Drain rounds between two yields to the tokio driver ([`ExtensionRuntime::wait_idle`]).
/// A round is one job / one schedular poll, so this only bounds pathological
/// self-sustaining chains; normal work streams through uninterrupted.
const DRAIN_YIELD_ROUNDS: usize = 64;

/// Upper bound on one park slice in the drain (see [`ExtensionRuntime::wait_idle`]).
/// Small enough that a due timer fires promptly even if a wakeup is lost,
/// large enough that an idle-but-live loop costs ~40 wakeups/s.
const DRAIN_PARK_SLICE: std::time::Duration = std::time::Duration::from_millis(25);

/// Ceiling of the adaptive park backoff (see [`ExtensionRuntime::spawn_drive_loop`]).
///
/// An idle-but-live extension runtime has no work to drain between lost-wakeup
/// fallback checks, so the fallback doubles from [`DRAIN_PARK_SLICE`] up to
/// this ceiling: at most ONE wakeup/s per extension instead of 40/4 on the
/// constrained deployment targets (the section 6.1 budget is <2 wakeups/s). Work
/// found in a drain resets the slice immediately, so a live extension is never
/// delayed by the backoff: a wakeup from the schedular still parks the task
/// only until the next due timer or queued job.
const DRAIN_PARK_SLICE_MAX: std::time::Duration = std::time::Duration::from_secs(1);

/// Next park slice after one drain: reset on work, double (up to the ceiling)
/// when the drain found nothing. Pure so the backoff is unit-testable.
fn next_park_slice(current: std::time::Duration, had_work: bool) -> std::time::Duration {
    if had_work {
        return DRAIN_PARK_SLICE;
    }
    current.saturating_mul(2).min(DRAIN_PARK_SLICE_MAX)
}

/// Test seam: the drain park backoff (integration tests live in `tests/`;
/// inline test modules are forbidden in `src/`).
#[doc(hidden)]
pub fn next_park_slice_for_tests(
    current: std::time::Duration,
    had_work: bool,
) -> std::time::Duration {
    next_park_slice(current, had_work)
}

/// Test seam: the `(base, ceiling)` drain park slices.
#[doc(hidden)]
pub fn park_slice_bounds_for_tests() -> (std::time::Duration, std::time::Duration) {
    (DRAIN_PARK_SLICE, DRAIN_PARK_SLICE_MAX)
}

/// Deadline-map token owned by the drive loop for one background drain. The
/// timed calls use monotonically increasing tokens from 0, so `u64::MAX` can
/// never collide with a command-owned deadline.
const BACKGROUND_JOB_DEADLINE_TOKEN: u64 = u64::MAX;

/// A background JS job running at least this long is reported under
/// `PI_TIMING=1` with the host operations it performed (see
/// [`host_op_stats`]).
const SLOW_BACKGROUND_JOB: std::time::Duration = std::time::Duration::from_millis(200);

/// Process-wide source of disjoint extension-id ranges. Each
/// [`ExtensionRuntime`] takes its own range, so an extension id captured by
/// one generation can never name a different generation's extension after
/// `/reload` (the shared `registered_ext_ids` set is cleared and repopulated
/// in place). `2^20` ids per generation keeps every id exact in QuickJS's
/// f64 numbers for roughly 8.6 billion runtime generations.
#[cfg(feature = "extensions")]
static NEXT_EXT_ID_BASE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "extensions")]
const EXT_ID_RANGE: u64 = 1 << 20;

/// Stable per-runtime generation key for the fs descriptor table.
///
/// `allowed_roots` is allocated once per [`ExtensionRuntime`] and kept alive
/// by that runtime, so its address identifies the runtime while any of its
/// descriptors can still exist. A reload can defer the old runtime's `Drop`
/// (in-flight command tasks hold `Rc`s), and that `Drop` must release only
/// the descriptors its own generation opened — see
/// [`state::FdTable::clear_generation`]. Address reuse after a runtime is
/// gone is safe: its own `Drop` already removed every entry tagged with the
/// key.
#[cfg(feature = "extensions")]
fn fd_generation(allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>) -> u64 {
    Arc::as_ptr(allowed_roots) as usize as u64
}

/// Number of actor command tasks currently executing (0 = the actor can only
/// be running background JS, if anything).
fn commands_in_flight(state: &Arc<StdMutex<RuntimeState>>) -> usize {
    lock_state(state)
        .commands_in_flight
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// Bounded log of unhandled-promise-rejection reason texts (the host
/// promise-rejection tracker's record).
///
/// INVARIANT (bounded, FIFO): the log never holds more than [`Self::MAX`]
/// reasons. The extension host NEVER drains it — only the `pir js` script
/// runner does — so without the cap a long-lived session that keeps producing
/// unhandled rejections (a fire-and-forget fetch against a dead endpoint, a
/// rejected timer callback) would retain one reason text, stack included, per
/// rejection for the whole process lifetime. Past the cap the OLDEST reasons
/// are evicted: the crash contract only needs the record to be non-empty, and
/// the newest reasons are the useful ones.
///
/// A `VecDeque` (not a `Vec`) keeps both ends O(1): the eviction here and the
/// FIFO removal on the `rejectionHandled` path ([`Self::resolved`]), which
/// used to be a `Vec::remove(0)` shift — quadratic over a rejection storm.
#[derive(Debug, Default)]
pub struct RejectionLog {
    reasons: VecDeque<String>,
}

impl RejectionLog {
    /// Cap on retained reasons.
    pub const MAX: usize = 1024;

    /// Record one unhandled rejection reason as the newest entry, evicting the
    /// oldest once the cap is reached.
    pub fn record(&mut self, reason: String) {
        if self.reasons.len() >= Self::MAX {
            self.reasons.pop_front();
        }
        self.reasons.push_back(reason);
    }

    /// A handler attached to a previously unhandled rejection (Node's
    /// `rejectionHandled`): drop the oldest record (FIFO; an empty log means
    /// the unhandled event was never recorded).
    pub fn resolved(&mut self) {
        self.reasons.pop_front();
    }

    /// Take every retained reason, oldest first, leaving the log empty.
    pub fn take(&mut self) -> Vec<String> {
        std::mem::take(&mut self.reasons).into_iter().collect()
    }

    /// Number of retained reasons.
    pub fn len(&self) -> usize {
        self.reasons.len()
    }

    /// Whether no reason is retained.
    pub fn is_empty(&self) -> bool {
        self.reasons.is_empty()
    }
}

/// A future that parks the current task until something wakes it: `Pending` on
/// the first poll, ready on the next. Unlike `tokio::task::yield_now` it never
/// re-schedules itself, so an idle drain parks (letting the tokio driver turn
/// its timer wheel) instead of spinning; the task is only polled again when
/// another waker — the schedular's queue waker, i.e. a runnable handle — fires.
struct ParkOneWake(bool);

impl std::future::Future for ParkOneWake {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.0 {
            std::task::Poll::Ready(())
        } else {
            self.0 = true;
            std::task::Poll::Pending
        }
    }
}

impl ExtensionRuntime {
    /// Create a runtime and inject the host function table + prelude.
    pub async fn new(config: ExtensionConfig) -> rquickjs::Result<Self> {
        let state = Arc::new(StdMutex::new(RuntimeState::new(config.clone())));
        Self::new_with_state(config, state).await
    }

    /// Spawn the executor drive loop on the current `LocalSet`.
    ///
    /// The rquickjs executor only runs spawned futures (a JS `await` on a
    /// host `JsAsync` promise, e.g. `ctx.waitForIdle`) while something drives
    /// the runtime. Opportunistic driving from `MaybePromise::into_future`
    /// polls stops the moment another `async_with` (an eval or renderer call)
    /// interleaves, which permanently starves the suspended future. A
    /// dedicated drive loop keeps every pending executor future alive
    /// regardless of interleaving. Must run inside the actor's `LocalSet`
    /// (rquickjs futures are `!Send`); `drive` resolves when the runtime is
    /// dropped.
    pub(crate) fn spawn_drive_loop(&self) {
        // The bounded-drain + park discipline [`Self::wait_idle`] documents:
        // a raw `drive()` keeps polling the executor without ever parking the
        // task, so tokio never advances its timer wheel and an armed
        // `setTimeout` (the prelude's `__pir_host.sleep`, a `tokio::time::sleep`)
        // never fires while the actor is otherwise idle — measured with a 2 s
        // background timer that only ran when an unrelated 3 s call timeout
        // woke the actor. Draining in bounded rounds and parking between them
        // lets the timer wheel turn; the weak handle ends the loop when the
        // actor drops the runtime.
        let weak = self.js_runtime.weak();
        let state = Arc::clone(&self.state);
        let deadlines = Arc::clone(&self.deadlines);
        tokio::task::spawn_local(async move {
            // Adaptive fallback slice: doubles while drains find nothing (an
            // idle runtime only needs the lost-wakeup safety net) and resets
            // to the base slice as soon as a drain runs work.
            let mut park_slice = DRAIN_PARK_SLICE;
            loop {
                let Some(drive) = weak.try_ref() else {
                    return;
                };
                // A drain with no command task executing is BACKGROUND work
                // (a promise continuation left behind by a finished command).
                // It runs under the per-call budget so it can never hold the
                // actor forever, and the interrupt handler additionally yields
                // it to a queued host command after the preemption grace.
                let background_started = (commands_in_flight(&state) == 0).then(|| {
                    let timeout = lock_state(&state).call_timeout;
                    lock_state(&deadlines).insert(
                        BACKGROUND_JOB_DEADLINE_TOKEN,
                        std::time::Instant::now() + timeout,
                    );
                    std::time::Instant::now()
                });
                let mut rounds = 0usize;
                let mut interrupted = false;
                loop {
                    match drive.execute_pending_job().await {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(_) => {
                            interrupted = true;
                            break;
                        }
                    }
                    rounds += 1;
                    if rounds.is_multiple_of(DRAIN_YIELD_ROUNDS) {
                        tokio::task::yield_now().await;
                    }
                }
                if background_started.is_some() {
                    lock_state(&deadlines).remove(&BACKGROUND_JOB_DEADLINE_TOKEN);
                }
                if interrupted {
                    // The pending exception is recovered by the next command
                    // task (or `/reload`); keep the runtime parked meanwhile.
                    lock_state(&state)
                        .background_interrupted
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    if crate::config::timings::timing_enabled_cached() {
                        let elapsed = background_started
                            .map(|started| started.elapsed())
                            .unwrap_or_default();
                        if let Some(summary) = host_op_stats::take_summary() {
                            eprintln!(
                                "pir-timing: ext.background_job: interrupted after {}ms (host ops: {summary})",
                                elapsed.as_millis()
                            );
                        } else {
                            eprintln!(
                                "pir-timing: ext.background_job: interrupted after {}ms",
                                elapsed.as_millis()
                            );
                        }
                    }
                } else if let Some(started) = background_started {
                    let elapsed = started.elapsed();
                    if elapsed >= SLOW_BACKGROUND_JOB {
                        if let Some(summary) = host_op_stats::take_summary() {
                            eprintln!(
                                "pir-timing: ext.background_job: {}ms (host ops: {summary})",
                                elapsed.as_millis()
                            );
                        } else {
                            eprintln!("pir-timing: ext.background_job: {}ms", elapsed.as_millis());
                        }
                    }
                }
                drop(drive);
                // `rounds` counts executed jobs: a drain with none backs the
                // fallback off; any work resets it.
                park_slice = next_park_slice(park_slice, rounds > 0);
                tokio::select! {
                    _ = ParkOneWake(false) => {}
                    _ = tokio::time::sleep(park_slice) => {}
                }
            }
        });
    }

    /// Create a runtime around an externally supplied shared state.
    ///
    /// The state mutex is shared verbatim with the extension handle actor
    /// ([`crate::extensions::ExtensionHandle`]), which needs synchronous
    /// read/write access to registry and flag data from arbitrary host
    /// threads. This constructor is `pub(crate)` for exactly that actor;
    /// outside code uses [`ExtensionRuntime::new`].
    pub(crate) async fn new_with_state(
        config: ExtensionConfig,
        state: Arc<StdMutex<RuntimeState>>,
    ) -> rquickjs::Result<Self> {
        // The single QuickJS construction path: count it for the builtin-only
        // startup regression (a builtin-only plan must never reach here).
        super::record_js_runtime_build();
        let cache: SourceCache = Arc::default();
        let allowed_roots: Arc<StdMutex<Vec<PathBuf>>> = Arc::default();
        // One generation key per runtime: the fd table and every process-level
        // host table (children/workers/fetches/servers/net) are scoped by it,
        // so a reload's deferred teardown can never release its successor's
        // resources.
        let generation = fd_generation(&allowed_roots);
        // Resolve the resource limits before the runtime exists: an explicit
        // `config.limits` is used verbatim; `None` resolves the GLOBAL
        // settings file overridden by `PI_EXT_*` env (project settings are
        // deliberately ignored — a repository must not loosen the host's own
        // engine limits). See `crate::extensions::limits`.
        let limits = ExtensionLimits::resolve(config.limits.clone());
        let rt = AsyncRuntime::new()?;
        // The default 1 MiB QuickJS stack RangeErrors far too early for real
        // workloads (acorn's recursive-descent parser over large sources,
        // deep JSON trees — the same motivation that gave workers 32 MiB).
        // The actor thread's OS stack is derived from this budget
        // (`ExtensionLimits::actor_stack_bytes`: 2x + 8 MiB headroom, 40 MiB
        // at the default), so 16 MiB of JS stack (the default;
        // `extensionLimits.stackMb` / PI_EXT_STACK_MB can lower it) is safely
        // inside it.
        // Clamped again at the engine call: an explicitly injected stack of 0
        // or above 16 MiB would otherwise read to rquickjs as "no stack
        // check", and deep recursion could then crash the process.
        rt.set_max_stack_size(
            limits
                .stack_bytes
                .clamp(1, crate::extensions::limits::MAX_STACK_BYTES),
        )
        .await;
        // Heap cap (unset = QuickJS unlimited): an over-cap allocation makes
        // QuickJS throw out of memory instead of growing the host process.
        // The dispatch/tool call then fails with an actionable error
        // (`memory_limit_error_message`) while the session stays usable.
        if let Some(memory_bytes) = limits.memory_bytes {
            rt.set_memory_limit(memory_bytes).await;
        }
        if let Some(gc_threshold) = limits.gc_threshold_bytes {
            rt.set_gc_threshold(gc_threshold).await;
        }
        rt.set_loader(
            ExtensionModuleLoader::new(cache.clone(), allowed_roots.clone()),
            ExtensionModuleLoader::new(cache.clone(), allowed_roots.clone()),
        )
        .await;
        // Interrupt-handler deadline: while timed calls are in flight the
        // handler aborts JS execution once ANY armed deadline has passed, so
        // a runaway loop does not just outlive its tokio timeout — it is
        // actually stopped and the runtime stays usable. Deadlines are
        // owner-tagged (P1-4): overlapping timed calls keep their own entry.
        //
        // The same handler enforces actor responsiveness against BACKGROUND
        // JS: a promise continuation left behind by a finished command runs on
        // the actor thread with no command-owned deadline. Once a host command
        // is queued ([`RuntimeState::pending_commands`]) and no command task
        // is executing, the continuation is interrupted after
        // [`super::BACKGROUND_PREEMPT_GRACE`] so the command (a prompt, a tool
        // call, a UI action) is never held hostage by extension background
        // work. The interrupted job's rejection is delivered to its promise
        // chain (the extension sees a clear interrupt error) and the drive
        // loop flags the runtime for recovery before the next command.
        let deadlines: Arc<StdMutex<HashMap<u64, std::time::Instant>>> = Arc::default();
        let (preempt_at, pending_commands, commands_in_flight) = {
            let state = lock_state(&state);
            (
                Arc::clone(&state.preempt_background_at_ms),
                Arc::clone(&state.pending_commands),
                Arc::clone(&state.commands_in_flight),
            )
        };
        {
            let deadlines = deadlines.clone();
            rt.set_interrupt_handler(Some(Box::new(move || {
                {
                    let guard = deadlines
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if guard
                        .values()
                        .any(|deadline| std::time::Instant::now() >= *deadline)
                    {
                        return true;
                    }
                }
                let preempt_at = preempt_at.load(std::sync::atomic::Ordering::Relaxed);
                if preempt_at != 0
                    && commands_in_flight.load(std::sync::atomic::Ordering::Relaxed) == 0
                    && pending_commands.load(std::sync::atomic::Ordering::Relaxed) > 0
                    && super::monotonic_millis() >= preempt_at
                {
                    return true;
                }
                false
            })))
            .await;
        }
        let ctx = AsyncContext::full(&rt).await?;

        // Host promise-rejection tracker: record reason texts of rejections
        // that reach the engine without a handler (a later handler-attach
        // event, `is_handled == true`, removes the entry). The `pir js`
        // script runner consumes the record for Node's crash contract; the
        // extension host only collects — which is exactly why the record is a
        // bounded [`RejectionLog`] rather than a growing `Vec`.
        let unhandled_rejections: Arc<StdMutex<RejectionLog>> = Arc::default();
        {
            let tracker_queue = unhandled_rejections.clone();
            rt.set_host_promise_rejection_tracker(Some(Box::new(
                move |ctx: rquickjs::Ctx<'_>,
                      _promise: rquickjs::Value<'_>,
                      reason: rquickjs::Value<'_>,
                      is_handled: bool| {
                    let text = rejection_reason_text(&ctx, &reason);
                    let Ok(mut queue) = tracker_queue.lock() else {
                        return;
                    };
                    if is_handled {
                        queue.resolved();
                    } else {
                        queue.record(text);
                    }
                },
            )))
            .await;
        }

        ctx.async_with(async |ctx| {
            register_host_functions(&ctx, &state, &allowed_roots, generation)?;
            // Node-compat globals (`__pirPlatform`, `process`) must exist
            // before any extension module body runs.
            ctx.eval::<(), _>(crate::extensions::api::node_shims::globals_source())
                .map_err(|e| {
                    rquickjs::Error::new_loading_message("pir-node-globals", e.to_string())
                })?;
            // The prelude is embedded deflated: inflate (once per process,
            // cached) here, at extension-host boot.
            ctx.eval::<(), _>(HOST_PRELUDE.source()).map_err(|e| {
                rquickjs::Error::new_loading_message(HOST_PRELUDE_MODULE, e.to_string())
            })?;
            // Pre-import every shimmed builtin into `__pirNodeModules` so the
            // synchronous CJS `require("path")` works without awaiting; the
            // dynamic import promise is awaited here (one boot-time sweep).
            ctx.eval::<(), _>(crate::extensions::api::node_shims::node_modules_bootstrap_source())
                .map_err(|e| {
                    rquickjs::Error::new_loading_message("pir-node-builtins", e.to_string())
                })?;
            let load_builtins: rquickjs::Function = ctx.globals().get("__pirLoadNodeBuiltins")?;
            let pending: MaybePromise = load_builtins.call(())?;
            pending.into_future::<String>().await?;

            // Host-SDK discovery + re-entry paths, published before any
            // extension module body runs. pi-subagents captures both at
            // module-evaluation time (`resolveAsyncPiPackageRoot()` at import,
            // `process.env` reads at spawn):
            //
            // 1. `PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT` — pi-subagents
            //    finds its host SDK through `process.argv[1]` (empty here: the
            //    extension runtime is not a node CLI), `import.meta.resolve`
            //    (no such surface in QuickJS) and finally this override, then
            //    fails the background launch closed when all three come up
            //    empty. The root is the host package pir materializes for
            //    exactly this contract.
            // 2. `PIR_EXECUTABLE` — the SDK child-session bridge re-enters the
            //    hosting image (`pir print --mode json`) for every child
            //    session; a host that knows its own executable publishes it
            //    rather than relying on `pir` being on PATH.
            // Both are additive: an operator-set value always wins.
            match crate::extensions::loader::ensure_host_pi_package_root(
                &crate::coding_agent::context::agent_dir(),
            ) {
                Ok(root) => {
                    if let Ok(mut roots) = allowed_roots.lock() {
                        roots.push(root.clone());
                    }
                    let root_text = root.display().to_string();
                    let executable = std::env::current_exe()
                        .ok()
                        .map(|path| path.display().to_string());
                    let script = host_env_boot_script(&root_text, executable.as_deref());
                    ctx.eval::<(), _>(script).map_err(|error| {
                        rquickjs::Error::new_loading_message("pir-host-env-boot", error.to_string())
                    })?;
                }
                // A read-only agent dir must not break extension boot: the
                // launch path then fails closed with pi-subagents' own
                // diagnostic.
                Err(error) => {
                    crate::ext_diag!("pir-ext: host pi package provisioning failed: {error}")
                }
            }
            Ok::<_, JsError>(())
        })
        .await?;

        // Publish the resolved limits only after the runtime booted
        // successfully, so a failed construction (e.g. a rebuild during
        // `/reload`) leaves the shared state untouched. A reload re-applies
        // the configured values, so a runtime `set_call_timeout` override is
        // not sticky across reloads.
        {
            let mut state = lock_state(&state);
            state.before_agent_start_timeout = limits.before_agent_start_timeout;
            state.limits = limits;
            state.call_timeout = state.limits.call_timeout;
            state.hook_timeout = state.limits.hook_timeout;
            state.memory_pressure_warned = false;
            state.memory_pressure_last_sample = None;
            // No table-wide generation to claim: each descriptor carries the
            // opening runtime's own key (see `state::FdTable`), passed to the
            // host functions above, so a predecessor's deferred `Drop` can
            // never close the successor's descriptors and vice versa.
        }

        Ok(Self {
            ctx,
            js_runtime: rt,
            cache,
            allowed_roots,
            generation,
            state,
            loaded: StdMutex::new(Vec::new()),
            next_ext_id: std::sync::atomic::AtomicU64::new(
                NEXT_EXT_ID_BASE.fetch_add(EXT_ID_RANGE, std::sync::atomic::Ordering::Relaxed),
            ),
            deadlines,
            next_deadline_token: std::sync::atomic::AtomicU64::new(0),
            unhandled_rejections,
        })
    }

    /// Seed a transpiled module source (primarily for tests).
    pub fn seed_module(&self, module_name: &str, ts_source: &str) -> Result<(), String> {
        let js = super::transpile::transpile(module_name, ts_source)?;
        self.cache
            .lock()
            .map_err(|_| "cache poisoned".to_string())?
            .insert(module_name.to_string(), js);
        Ok(())
    }

    /// Seed an already-transpiled module source (JavaScript, not TypeScript).
    /// Used by the `pir js` script runner to install the entry module with
    /// the top-level-await downgrade applied (`loader::script_entry_module_js`);
    /// the module loader hits this cache before reading the file.
    pub fn seed_module_js(&self, module_name: &str, js_source: &str) -> Result<(), String> {
        self.cache
            .lock()
            .map_err(|_| "cache poisoned".to_string())?
            .insert(module_name.to_string(), js_source.to_string());
        Ok(())
    }

    /// Load an already-seeded module by name (test hook). Returns 1 on
    /// success.
    pub async fn seeded_load(&self, module_name: &str) -> anyhow::Result<usize> {
        let ext_id = self
            .next_ext_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = module_name
            .strip_prefix(super::loader::EXT_MODULE_PREFIX)
            .unwrap_or(module_name);
        lock_state(&self.state).ext_identities.insert(
            ext_id,
            sanitized_extension_identity(std::path::Path::new(path)),
        );
        let payload = self
            .load_payload(
                "__pir_loadExtension",
                ext_id.to_string(),
                module_name.to_string(),
                path.to_string(),
            )
            .await?;
        let parsed: JsonValue = serde_json::from_str(&payload)?;
        if parsed
            .get("ok")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false)
        {
            self.refresh_registry().await?;
            Ok(1)
        } else {
            let error = parsed
                .get("error")
                .and_then(JsonValue::as_str)
                .unwrap_or("unknown error")
                .to_string();
            Err(anyhow::anyhow!("extension failed to load: {error}"))
        }
    }

    /// Evaluate a JS expression expected to produce a JSON string (test hook).
    ///
    /// Failure reporting: rquickjs's Display for an engine exception is the
    /// bare "Exception generated by QuickJS", which cannot tell a `SyntaxError`
    /// (a raw-eval script that is not valid JavaScript — `eval_json` runs the
    /// text as-is, with no transpile) from a call-deadline interrupt. The
    /// pending exception's own text is therefore appended, and the exception
    /// is cleared as it is read so the runtime stays usable for the next eval.
    pub async fn eval_json(&self, code: &str) -> anyhow::Result<serde_json::Value> {
        let json: String = match self
            .ctx
            .async_with(async |ctx| ctx.eval::<String, _>(code.to_string()))
            .await
        {
            Ok(json) => json,
            Err(error) => {
                let text = self.take_pending_exception_text().await;
                return Err(anyhow::anyhow!(match text {
                    Some(text) => format!("{error}: {text}"),
                    None => error.to_string(),
                }));
            }
        };
        Ok(serde_json::from_str(&json)?)
    }

    // -- Script mode (`pir js`, see `main_cli/js_mode.rs`) -------------------
    //
    // The script runner reuses this runtime verbatim — same host function
    // table, same prelude, same loader — but never calls `load` (no extension
    // discovery/scan). The four methods below are its whole surface: install
    // the script boot overlay, import the entry module, drain the event loop
    // to quiescence, and read the final `process.exitCode`.

    /// Evaluate the script-mode boot snippet (argv/execPath, real-stdio
    /// console, `process.exit`). Must run after the standard boot so it can
    /// override the prelude's notify-routed console.
    pub async fn eval_boot(&self, source: &str) -> anyhow::Result<()> {
        self.ctx
            .async_with(async |ctx| ctx.eval::<(), _>(source.to_string()))
            .await
            .map_err(|error| anyhow::anyhow!("script boot failed: {error}"))
    }

    /// Import the script entry module by its loader name (`pir-ext:<path>`)
    /// and await its completion (top-level await included). Module-level
    /// errors come back as the formatted stack text instead of `Err` — the
    /// runner prints them on stderr and exits 1, like Node's uncaught
    /// exception path. `Err` is reserved for runtime-level failures (the
    /// entry could not even be called).
    pub async fn run_script(&self, module_name: &str) -> anyhow::Result<Result<(), String>> {
        let payload: String = self
            .ctx
            .async_with(async |ctx| -> rquickjs::Result<String> {
                let run: rquickjs::Function = ctx.globals().get("__pirScriptRun")?;
                let pending: MaybePromise = run.call((module_name.to_string(),))?;
                pending.into_future::<String>().await
            })
            .await
            .map_err(|error| anyhow::anyhow!("script execution failed: {error}"))?;
        let parsed: JsonValue = serde_json::from_str(&payload)
            .map_err(|error| anyhow::anyhow!("script outcome parse failed: {error}"))?;
        if parsed
            .get("ok")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false)
        {
            return Ok(Ok(()));
        }
        let stack = parsed
            .get("stack")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(Err(stack))
    }

    /// Drain the runtime to quiescence: every pending job, microtask and
    /// spawned host future (timers, fetch poll loops, child-process readers)
    /// runs to completion. This is the script runner's event-loop drain —
    /// the point Node exits when the loop goes empty. A repeating `setInterval`
    /// keeps it pending forever, matching Node.
    ///
    /// rquickjs' `AsyncRuntime::idle()` is deliberately NOT used here: that
    /// drive loop runs to completion inside a single poll and never yields to
    /// the tokio driver. Every pir timer rides that driver — the prelude's
    /// `setTimeout`/`setInterval` await `__pir_host.sleep`, i.e.
    /// `tokio::time::sleep` (see `host_functions::context_fns`) — and tokio
    /// only advances its timer wheel while the runtime parks. A drive loop
    /// that keeps polling without ever parking therefore starves every armed
    /// timer while the script's non-timer host ops (fs, dynamic import,
    /// child-process spawn) keep progressing: the reported `pir js` symptom of
    /// "top-level evaluation ended, timers stopped being delivered".
    ///
    /// The drain below drives in bounded rounds and parks the task between
    /// them, so the timer wheel keeps turning and an armed `setTimeout` fires
    /// while a long-lived chain is still running. Quiescence is unchanged:
    /// with no pending job and no live spawned future the drain returns at
    /// once, exactly where Node's event loop would go empty.
    pub async fn wait_idle(&self) {
        loop {
            // 1) Run jobs and drive spawned host futures in bounded rounds: a
            //    self-sustaining chain (a job that keeps re-arming itself, a
            //    future that re-queues on every poll) must not hold one poll
            //    forever, or the driver never gets its turn again.
            let mut rounds = 0usize;
            loop {
                // A job that threw reports `true`: its outcome reaches the
                // script through the unhandled-rejection tracker, not through
                // this drain.
                let progressed = self.js_runtime.execute_pending_job().await.unwrap_or(true);
                if !progressed {
                    break;
                }
                rounds += 1;
                if rounds.is_multiple_of(DRAIN_YIELD_ROUNDS) {
                    tokio::task::yield_now().await;
                }
            }
            // 2) Nothing is pending: the loop is empty, Node would exit here.
            if !self.js_runtime.is_job_pending().await {
                return;
            }
            // 3) Live handles, none runnable *right now*: an armed timer
            //    waiting for its deadline, a child reader waiting for bytes, a
            //    fetch poll waiting for a socket. Park until one wakes this
            //    task — the schedular's queue waker IS this task's waker (the
            //    poll in step 1 registered it), so a runnable handle resumes
            //    the drain immediately. The slice is the safety net: it bounds
            //    any lost wakeup and keeps the timer wheel turning (a due
            //    timer fires at its deadline + at most one slice).
            tokio::select! {
                _ = ParkOneWake(false) => {}
                _ = tokio::time::sleep(DRAIN_PARK_SLICE) => {}
            }
        }
    }

    /// Take the promise rejections the engine reported still unhandled
    /// (reason texts, oldest first, at most [`RejectionLog::MAX`] of them —
    /// see [`RejectionLog`]). The script runner treats a non-empty list as
    /// Node's unhandled-rejection crash; the extension host collects without
    /// acting.
    pub async fn take_unhandled_rejections(&self) -> Vec<String> {
        match self.unhandled_rejections.lock() {
            Ok(mut log) => log.take(),
            Err(_) => Vec::new(),
        }
    }

    /// Read the effective exit code (`process.exitCode`, coerced like Node:
    /// numbers are truncated, invalid values fall back to 0).
    pub async fn read_exit_code(&self) -> anyhow::Result<i32> {
        let code: String = self
            .ctx
            .async_with(async |ctx| {
                ctx.eval::<String, _>("String(Number(process.exitCode ?? 0))".to_string())
            })
            .await
            .map_err(|error| anyhow::anyhow!("exit code read failed: {error}"))?;
        Ok(code.trim().parse::<i32>().unwrap_or(0))
    }

    /// Allow on-disk module loading below the given root directory.
    pub fn allow_root(&self, root: PathBuf) {
        if let Ok(mut roots) = self.allowed_roots.lock() {
            roots.push(root);
        }
    }

    /// Load extensions from the given paths (files or directories).
    /// Factories run sequentially; a failing extension produces an error
    /// entry, never a fatal error (spec §4.2).
    pub async fn load(&self, paths: &[PathBuf]) -> anyhow::Result<LoadResult> {
        let load_started = crate::config::timings::start_phase();
        for path in paths {
            let root = if path.is_file() {
                package_entry_root(path)
            } else {
                path.clone()
            };
            self.allow_root(root);
        }

        let discovery = discover_extensions(paths);
        let mut result = LoadResult {
            loaded: Vec::new(),
            errors: discovery.errors,
        };

        'ext: for ext in &discovery.extensions {
            // Seed the entry module cache with a transpile up front so load
            // errors surface as `LoadResult.errors` rather than side-channel
            // logs. A CommonJS or JSON entry gets the same wrapper the loader
            // uses for dependencies: an ESM transpile of a CJS file would die
            // on `module is not defined` (commands like `-e ./dir` pointing at
            // a CJS index.js must load like upstream).
            //
            // Kind and text come from one classification pass: for an
            // ambiguous `.js` (nearest manifest carries no `"type"`) the
            // classification reads the file to decide ESM vs CJS and hands
            // that text back, so the seed does not read the file again for
            // the kind, and the CJS arm below reuses the same text instead of
            // the lexer reading the root file a third time. Every other
            // spelling keeps the single seed read it always did.
            let (kind, classified) = super::loader::module_kind_and_source(&ext.path);
            if let Some(source) = classified.or_else(|| std::fs::read_to_string(&ext.path).ok()) {
                let transpiled = match kind {
                    crate::extensions::loader::npm_resolve::ModuleKind::Cjs => {
                        let actual = ext.path.to_string_lossy().replace('\\', "/");
                        // The text in hand is the file's content (from the
                        // classification read or the seed read just above), so
                        // the lexer scans it directly; its re-export targets
                        // are still read by path, as they must be.
                        let names = crate::extensions::loader::cjs_lexer::named_exports_with_source(
                            &ext.path, source,
                        );
                        Ok(super::loader::cjs_interop_wrapper(&actual, &names))
                    }
                    crate::extensions::loader::npm_resolve::ModuleKind::Json => {
                        Ok(super::loader::json_default_wrapper(&source))
                    }
                    _ => super::transpile::transpile(&ext.module_name, &source),
                };
                match transpiled {
                    Ok(js) => {
                        // Dependency scan (upstream loader's purpose:
                        // reject node/npm deps and unknown bare specifiers
                        // before execution). Relative and known virtual
                        // modules are fine; anything else fails the load.
                        for dep in super::loader::scan_dependencies(&js) {
                            if !dep.is_supported() {
                                result.errors.push(ExtensionLoadError {
                                    path: ext.path.clone(),
                                    error: format!(
                                        "unsupported dependency `{}`: npm/node modules are not available in this runtime",
                                        dep.specifier
                                    ),
                                });
                                continue 'ext;
                            }
                        }
                        if let Ok(mut cache) = self.cache.lock() {
                            cache.insert(ext.module_name.clone(), js);
                        }
                    }
                    Err(e) => {
                        result.errors.push(ExtensionLoadError {
                            path: ext.path.clone(),
                            error: e,
                        });
                        continue;
                    }
                }
            }

            let ext_id = self
                .next_ext_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Sanitized identity for the `PI_TIMING=1` per-handler lines; the
            // full path can carry the user name and must never be logged.
            lock_state(&self.state)
                .ext_identities
                .insert(ext_id, sanitized_extension_identity(&ext.path));
            let payload = self
                .load_payload(
                    "__pir_loadExtension",
                    ext_id.to_string(),
                    ext.module_name.clone(),
                    ext.path.display().to_string(),
                )
                .await?;
            let parsed: JsonValue = serde_json::from_str(&payload)?;
            if parsed
                .get("ok")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false)
            {
                result.loaded.push(ext.clone());
                // The entry source seeded above is consumed by the module
                // loader; QuickJS caches the module record from here on, so
                // keeping the Rust-side copy only costs memory (the largest
                // entry measured ~1 MB). A reload re-seeds its own runtime.
                if let Ok(mut cache) = self.cache.lock() {
                    cache.remove(&ext.module_name);
                }
            } else {
                let error = parsed
                    .get("error")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("unknown error")
                    .to_string();
                result.errors.push(ExtensionLoadError {
                    path: ext.path.clone(),
                    error,
                });
            }
        }

        self.loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone_from(&result.loaded);
        lock_state(&self.state).load_paths = paths.to_vec();
        self.refresh_registry().await?;
        // `PI_TIMING=1`: one line with the resolved resource limits, so a
        // bug report carries the effective cap/stack/timeout configuration.
        if crate::config::timings::timing_enabled() {
            let limits = lock_state(&self.state).limits.clone();
            crate::ext_diag_info!("pir-ext: resolved limits: {}", limits.summary());
        }
        // Discovery + transpile + top-level evaluation of every on-disk
        // extension, once per start/validation load. `--only-builtin` never
        // reaches this function (its load list is empty).
        crate::config::timings::log_phase_once_opt(
            "ext.runtime.load",
            "ext.runtime.load",
            crate::config::timings::finish_phase(load_started),
        );
        Ok(result)
    }

    /// One `__pir_loadExtension` round trip with failure recovery: a load
    /// whose JS call itself fails (an uncatchable exception thrown while the
    /// module body ran — e.g. QuickJS's out-of-memory against the configured
    /// heap cap — or an interrupt) must not leave the pending exception
    /// behind. Recover so the runtime stays usable, and report the actionable
    /// limit text when the heap cap is the cause.
    async fn load_payload(
        &self,
        name: &str,
        a: String,
        b: String,
        c: String,
    ) -> anyhow::Result<String> {
        match self.invoke_str3(name, a, b, c).await {
            Ok(payload) => Ok(payload),
            Err(js_error) => {
                let memory = self.memory_limit_failure().await;
                self.recover_after_interrupt().await;
                Err(anyhow::anyhow!(
                    "{}",
                    memory.unwrap_or_else(|| js_error.to_string())
                ))
            }
        }
    }

    /// Snapshot of registered tools after the last load.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        lock_state(&self.state).tool_defs.clone()
    }

    /// Run a JS tool's `prepareLoadout` hook (upstream
    /// `ToolDefinition.prepareLoadout`): `loadout` is the
    /// [`crate::agent::tool_execution::ToolLoadout`] wire JSON. Returns `None`
    /// when the tool has no hook or the hook returned undefined; a throwing
    /// hook yields the error text.
    ///
    /// Reserved-prefix keys (`__pir_stream:` / `__pir_route:` — the JS
    /// `streamSimple` / virtual-model `route` callbacks, see
    /// [`crate::ai::providers::js_stream`]) run under the long
    /// [`HOOK_CALL_TIMEOUT`] instead of the configured per-call timeout: a
    /// real model backend can stream for many minutes. Real tool names are
    /// validated identifiers and can never carry the reserved prefix, so
    /// genuine `prepareLoadout` hooks keep the short timeout.
    pub async fn call_prepare_loadout(
        &self,
        tool_name: &str,
        loadout: &JsonValue,
    ) -> Result<Option<JsonValue>, String> {
        let loadout_json = serde_json::to_string(loadout)
            .map_err(|error| format!("loadout serialization failed: {error}"))?;
        let timeout = self.prepare_loadout_timeout(tool_name);
        let token = self.arm_deadline_for(timeout);
        let result = tokio::time::timeout(
            timeout,
            self.invoke_str2(
                "__pir_callPrepareLoadout",
                tool_name.to_string(),
                loadout_json,
            ),
        )
        .await;
        // Heap-cap diagnosis before the recovery drain (same as call_tool).
        let memory_failure = if !matches!(result, Ok(Ok(_))) {
            let failure = self.memory_limit_failure().await;
            // Disarm after the recovery drain (P1-4): recovered
            // continuations still run under an armed deadline.
            self.recover_after_interrupt().await;
            failure
        } else {
            self.sample_memory_pressure().await;
            None
        };
        self.disarm_deadline(token);
        let result_json = match result {
            Ok(Ok(result_json)) => result_json,
            Ok(Err(js_error)) => {
                self.recover_after_interrupt().await;
                return Err(memory_failure.unwrap_or_else(|| js_error.to_string()));
            }
            Err(_elapsed) => {
                self.recover_after_interrupt().await;
                return Err(format!("tool `{tool_name}` prepareLoadout timed out"));
            }
        };
        let parsed: JsonValue =
            serde_json::from_str(&result_json).map_err(|error| error.to_string())?;
        if parsed.get("error").and_then(JsonValue::as_str).is_some() {
            let error = parsed
                .get("error")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_owned();
            return Err(self.decorate_memory_error(error).await);
        }
        Ok((!parsed.is_null()).then_some(parsed))
    }
}

/// Boot snippet publishing the host-SDK discovery env for extensions.
///
/// Sanitized extension identity for the `PI_TIMING=1` per-handler lines.
///
/// A full extension path can contain the user's home directory, so the log
/// only carries the part that identifies the extension itself: the suffix
/// after the LAST `node_modules` component (scope/package + entry file), or —
/// for a plain file/directory entry — the last two path components. Output is
/// slash-normalized and capped at [`MAX_IDENTITY_CHARS`]; no prompts,
/// payloads or arguments are ever included.
pub(crate) fn sanitized_extension_identity(path: &std::path::Path) -> String {
    const MAX_IDENTITY_CHARS: usize = 96;
    let text = path.to_string_lossy().replace('\\', "/");
    let suffix = match text.rsplit_once("/node_modules/") {
        Some((_, tail)) => tail.to_owned(),
        None => {
            let components: Vec<&str> = text
                .split('/')
                .filter(|component| !component.is_empty())
                .collect();
            match components.len() {
                0 => "extension".to_owned(),
                1 => components[0].to_owned(),
                _ => components[components.len() - 2..].join("/"),
            }
        }
    };
    let suffix = suffix.trim_start_matches('/');
    if suffix.is_empty() {
        return "extension".to_owned();
    }
    if suffix.chars().count() <= MAX_IDENTITY_CHARS {
        suffix.to_owned()
    } else {
        // Keep the end (the entry file) rather than the beginning.
        suffix
            .chars()
            .skip(suffix.chars().count() - MAX_IDENTITY_CHARS)
            .collect()
    }
}

/// Runs against the `process` stub installed by the node-globals boot (its
/// `env` is a host snapshot of the real environment), so both assignments land
/// where extension code reads them — and, because pi-subagents rebuilds the
/// detached runner's environment from `process.env`, they reach the runner
/// process and its own children too. Existing values are never overwritten.
fn host_env_boot_script(package_root: &str, executable: Option<&str>) -> String {
    let root = serde_json::to_string(package_root).unwrap_or_else(|_| "\"\"".to_string());
    let executable = executable
        .and_then(|path| serde_json::to_string(path).ok())
        .unwrap_or_else(|| "undefined".to_string());
    format!(
        "(() => {{\n\
         \x20 const env = process.env;\n\
         \x20 const assign = (key, value) => {{\n\
         \x20   if (value === undefined) return;\n\
         \x20   const current = env[key];\n\
         \x20   if (typeof current === \"string\" && current !== \"\") return;\n\
         \x20   env[key] = value;\n\
         \x20 }};\n\
         \x20 assign(\"PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT\", {root});\n\
         \x20 assign(\"PIR_EXECUTABLE\", {executable});\n\
         }})();\n"
    )
}

/// Teardown of a runtime generation (session quit, or `/reload` replacing
/// it): this is the last moment the QuickJS context exists, and the only
/// teardown JS point the runtime owns. Two things happen here, in order:
///
/// 1. Node's final process event — `process.emit("exit", exitCode)` — is
///    evaluated synchronously (exit handlers must not await, per Node), so
///    extensions that flush on `exit` observe the teardown.
/// 2. Every fd of the fs v2 table opened by THIS generation is released: the
///    `RuntimeState` (and its fd table) is shared into a reload's fresh
///    runtime, and this `Drop` can run after the successor has already opened
///    descriptors — so the clear is generation-scoped, never a whole-table
///    clear.
impl Drop for ExtensionRuntime {
    fn drop(&mut self) {
        let _ = block_on(self.ctx.async_with(async |ctx| {
            let _ = ctx.eval::<(), _>(crate::extensions::api::node_shims::PROCESS_EXIT_SOURCE);
        }));
        self.release_host_resources();
    }
}

impl ExtensionRuntime {
    /// Release every resource this generation owns: its fs descriptors and the
    /// process-level host tables (child processes, worker threads, in-flight
    /// fetches, HTTP servers, net sockets and `fs.watch` threads).
    ///
    /// Idempotent and generation-scoped: called from [`Drop`] and explicitly
    /// by the actor immediately before `Shutdown`, because in-flight command
    /// `Rc`s can defer `Drop` past the shutdown reply. A generation key is
    /// never reused (the `allowed_roots` allocation stays alive for the whole
    /// `Drop`), so a late successor can never be released by mistake.
    pub(crate) fn release_host_resources(&self) {
        super::host_functions::release_host_generation(self.generation);
        lock_state(&self.state)
            .fd_table
            .clear_generation(self.generation);
    }
}

/// Minimal current-thread `block_on` for the teardown eval. The exit snippet
/// contains no awaits, so the with-future completes on the first poll; the
/// timeout budget is a safety net against a contaminated runtime pinning
/// process teardown (the exit event is best-effort, like Node's exit phase).
fn block_on<F: std::future::Future>(future: F) -> Option<F::Output> {
    use std::task::{Context as TaskContext, Poll, Wake, Waker};

    struct ParkWaker(std::thread::Thread);
    impl Wake for ParkWaker {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.unpark();
        }
    }

    let mut future = std::pin::pin!(future);
    let waker = Waker::from(std::sync::Arc::new(ParkWaker(std::thread::current())));
    let mut task = TaskContext::from_waker(&waker);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match future.as_mut().poll(&mut task) {
            Poll::Ready(value) => return Some(value),
            Poll::Pending => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::park_timeout(std::time::Duration::from_millis(10));
            }
        }
    }
}

/// Sandbox root for a file extension entry point.
///
/// Single-file entries keep their own directory. npm-package entries whose
/// manifest points deep inside a build layout (`context-mode`:
/// `build/adapters/pi/extension.js` importing `../../session/db.js`) span more
/// than that directory, so the nearest ancestor carrying a `package.json`
/// (within five levels) becomes the root instead — the same boundary Node's
/// package resolution treats as the package. Without a manifest nearby the
/// previous file-parent rule stands.
///
/// Public for the `pir js` script runner (`main_cli/js_mode.rs`), which seeds
/// the same roots for a standalone script entry.
pub fn package_entry_root(path: &Path) -> PathBuf {
    const MAX_LEVELS: usize = 5;
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut cursor = Some(parent);
    let mut levels = 0;
    while let Some(dir) = cursor {
        if levels >= MAX_LEVELS {
            break;
        }
        if dir.join("package.json").is_file() {
            return dir.to_path_buf();
        }
        cursor = dir.parent();
        levels += 1;
    }
    parent.to_path_buf()
}

/// Best-effort reason text for the rejection tracker: an Error's message +
/// stack, its message alone, then the JSON form of the value (never fails — a
/// stringify miss degrades to a placeholder). QuickJS stacks carry no message
/// header, so the two properties are joined by hand.
///
/// Shared with the failure diagnosis in `ops.rs`, which reads a pending JS
/// exception (a rejected promise's reason re-thrown onto the context) before
/// recovery discards it.
pub(super) fn rejection_reason_text<'js>(
    ctx: &rquickjs::Ctx<'js>,
    reason: &rquickjs::Value<'js>,
) -> String {
    if let Some(object) = reason.as_object() {
        let message = object.get::<_, Option<String>>("message").ok().flatten();
        let stack = object.get::<_, Option<String>>("stack").ok().flatten();
        if let Some(message) = message.filter(|m| !m.is_empty()) {
            return match stack {
                Some(stack) if !stack.is_empty() && !stack.contains(&message) => {
                    format!(
                        "{message}
{stack}"
                    )
                }
                _ => message,
            };
        }
        if let Some(stack) = stack.filter(|s| !s.is_empty()) {
            return stack;
        }
    }
    match ctx.json_stringify(reason.clone()) {
        Ok(Some(text)) => text.to_string().unwrap_or_default(),
        _ => String::from("(non-serializable rejection reason)"),
    }
}
