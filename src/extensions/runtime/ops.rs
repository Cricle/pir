use super::*;

/// Minimum spacing between the throttled heap-pressure samples (see
/// [`ExtensionRuntime::sample_memory_pressure`]).
const MEMORY_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Budget for draining broken continuations after a background interrupt
/// (see [`ExtensionRuntime::recover_background_interrupt`]).
const BACKGROUND_RECOVERY_TIMEOUT: Duration = Duration::from_secs(2);

impl ExtensionRuntime {
    /// Abort the in-flight extension tool call: the JS-side `signal` reports
    /// `aborted` and `throwIfAborted()` throws on its next check. The host
    /// re-arms the flag with [`ExtensionRuntime::clear_abort`] when a new run
    /// starts.
    pub fn abort_current_call(&self) {
        lock_state(&self.state).tool_call_aborted = true;
    }

    /// Clear the abort flag and reset JS-side abort listeners.
    pub async fn clear_abort(&self) {
        lock_state(&self.state).tool_call_aborted = false;
        let _ = self.invoke_0("__pir_clear_abort").await;
    }

    /// Drive pending JS jobs (and the spawned-host-future schedular) until
    /// neither makes progress (bounded). Resolving a promise from a spawned
    /// host future (e.g. a nested `ctx.executeTool` completing while another
    /// command task ran) can leave the awaiting JS continuation queued after
    /// the resolving command task finished; the actor sweeps after every
    /// command so suspended callers resume.
    ///
    /// The sweep runs under the per-call interrupt deadline: a continuation
    /// that starts a long synchronous turn here would otherwise run with no
    /// armed deadline (the command's own deadline is disarmed before this
    /// point). A job that exceeds the budget is interrupted and the pending
    /// exception is cleared; the runtime stays usable.
    pub(crate) async fn drain_pending_jobs(&self) {
        let token = self.arm_deadline();
        for _ in 0..4096 {
            // A queued host command outranks background continuations: stop
            // sweeping (the command task takes the actor next) instead of
            // spinning through instantly-preempted jobs.
            {
                let state = lock_state(&self.state);
                let pending = state
                    .pending_commands
                    .load(std::sync::atomic::Ordering::Relaxed);
                let in_flight = state
                    .commands_in_flight
                    .load(std::sync::atomic::Ordering::Relaxed);
                if pending > in_flight {
                    break;
                }
            }
            match self.js_runtime.execute_pending_job().await {
                Ok(true) => continue,
                Ok(false) => break,
                Err(_) => {
                    // An interrupt (the armed deadline) or a throwing job:
                    // clear the pending exception and keep sweeping (bounded
                    // by the loop count).
                    self.ctx
                        .async_with(async |ctx| {
                            if ctx.has_exception() {
                                let _ = ctx.catch();
                            }
                        })
                        .await;
                }
            }
        }
        self.disarm_deadline(token);
    }

    /// Flags registered during load.
    pub fn flags(&self) -> Vec<FlagDef> {
        lock_state(&self.state).flags.clone()
    }

    /// Current value of a registered flag (CLI value > default).
    pub fn get_flag(&self, name: &str) -> Option<FlagValue> {
        lock_state(&self.state).flag_value(name)
    }

    /// Apply a CLI-provided value to a registered flag.
    pub fn set_flag_value(&mut self, name: &str, value: FlagValue) -> Result<(), String> {
        lock_state(&self.state).set_flag_value(name, value)
    }
    /// Drain accumulated side-effect notifications.
    pub fn drain_notifications(&self) -> Vec<Notification> {
        let mut state = lock_state(&self.state);
        state.filter_abandoned_notifications();
        std::mem::take(&mut state.notifications)
            .into_iter()
            .map(|(_, notification)| notification)
            .collect()
    }

    // -- internals ----------------------------------------------------------

    /// The configured per-call timeout (read from the shared state so
    /// `ExtensionHandle::set_call_timeout` applies to subsequent calls).
    pub(super) fn call_timeout(&self) -> Duration {
        lock_state(&self.state).call_timeout
    }

    /// The configured reserved-prefix hook timeout (initialized from the
    /// resolved [`ExtensionLimits`]; default [`HOOK_CALL_TIMEOUT`]).
    pub(super) fn hook_timeout(&self) -> Duration {
        lock_state(&self.state).hook_timeout
    }

    /// Deadline for one `call_prepare_loadout` round trip. Reserved-prefix
    /// hook keys (`__pir_stream:` / `__pir_route:` — the JS `streamSimple` /
    /// virtual-model `route` callbacks) always get the long hook timeout
    /// (initialized from `extensionLimits.hookTimeoutMs` /
    /// `PI_EXT_HOOK_TIMEOUT_MS`, default [`HOOK_CALL_TIMEOUT`]), regardless of
    /// `set_call_timeout` (a real model backend can stream for many minutes);
    /// every other key is a genuine tool `prepareLoadout` hook and keeps the
    /// configured short timeout.
    pub(super) fn prepare_loadout_timeout(&self, tool_name: &str) -> Duration {
        use crate::ai::providers::{JS_ROUTE_HOOK_PREFIX, JS_STREAM_HOOK_PREFIX};
        if tool_name.starts_with(JS_STREAM_HOOK_PREFIX)
            || tool_name.starts_with(JS_ROUTE_HOOK_PREFIX)
        {
            self.hook_timeout()
        } else {
            self.call_timeout()
        }
    }

    // -- resource-limit diagnosis ------------------------------------------

    /// The configured heap cap, if any.
    pub(super) fn memory_limit(&self) -> Option<usize> {
        lock_state(&self.state).limits.memory_bytes
    }

    /// The actionable out-of-memory text when a heap cap is configured
    /// (`extension exceeded its JS memory limit (N MB); raise
    /// `extensionLimits.memoryMb` ... or set PI_EXT_MEMORY_MB`).
    pub(super) fn memory_limit_error(&self) -> Option<String> {
        let state = lock_state(&self.state);
        state
            .limits
            .memory_bytes
            .map(|_| state.limits.memory_limit_error_message())
    }

    /// Current JS heap usage in bytes: the larger of QuickJS's malloc total
    /// and its used-size accounting (both are what the cap is enforced
    /// against, and the peak one is the honest number to report).
    pub(super) async fn js_memory_used(&self) -> usize {
        let usage = self.js_runtime.memory_usage().await;
        usage.malloc_size.max(usage.memory_used_size).max(0) as usize
    }

    /// Take the pending JS exception's text (clearing it), if any.
    ///
    /// Must run BEFORE [`Self::recover_after_interrupt`] on a failed call:
    /// `Promise::result()` re-throws a rejection's reason onto the context,
    /// and recovery discards it — so this is the only window in which the
    /// original message (e.g. QuickJS's `out of memory`) can be read.
    pub(super) async fn take_pending_exception_text(&self) -> Option<String> {
        self.ctx
            .async_with(async |ctx| {
                Ok::<_, JsError>(if ctx.has_exception() {
                    let value = ctx.catch();
                    Some(super::runtime::rejection_reason_text(&ctx, &value))
                } else {
                    None
                })
            })
            .await
            .ok()
            .flatten()
    }

    /// Diagnose a failed JS call whose cause may be the configured heap cap:
    /// samples the exception text AND the heap usage before recovery callers
    /// run it, emits the once-per-crossing pressure diagnostic, and returns
    /// the actionable limit error when this failure is a heap-limit hit.
    pub(super) async fn memory_limit_failure(&self) -> Option<String> {
        let limit = self.memory_limit()?;
        let used = self.js_memory_used().await;
        let exception = self.take_pending_exception_text().await;
        self.note_memory_pressure(used, limit);
        let exhausted = exception
            .as_deref()
            .is_some_and(ExtensionLimits::looks_like_memory_exhaustion)
            || heap_at_limit(used, limit);
        if !exhausted {
            return None;
        }
        // Reclaim the failed call's garbage before the next dispatch: the
        // interrupted continuation may still hold the allocations that
        // exhausted the heap, and the next call must not fail only because
        // the corpse is still reachable. `recover_after_interrupt` (run by
        // the caller right after) drops the pending exception and the broken
        // job queue; the GC runs here while the peak is still observable for
        // the diagnostic above.
        self.js_runtime.run_gc().await;
        self.memory_limit_error()
    }

    /// Throttled heap sample for completed dispatch/tool calls: emits the
    /// pressure diagnostic if the heap crossed 80% of the configured cap (a
    /// no-op without a cap). `memory_usage()` walks every allocation, and
    /// `emit` runs once per provider stream chunk, so the walk is bounded to
    /// one call per [`MEMORY_SAMPLE_INTERVAL`]; the failure path samples
    /// unthrottled (`memory_limit_failure`), where the cost is irrelevant.
    pub(super) async fn sample_memory_pressure(&self) {
        let Some(limit) = self.memory_limit() else {
            return;
        };
        {
            let mut state = lock_state(&self.state);
            let now = std::time::Instant::now();
            if state
                .memory_pressure_last_sample
                .is_some_and(|last| now.duration_since(last) < MEMORY_SAMPLE_INTERVAL)
            {
                return;
            }
            state.memory_pressure_last_sample = Some(now);
        }
        let used = self.js_memory_used().await;
        self.note_memory_pressure(used, limit);
    }

    /// One diagnostic per crossing: `>= 80%` logs once (with the raw usage
    /// numbers) and arms the one-shot; dropping back below the threshold
    /// re-arms it, so a heap that keeps hovering near the cap cannot flood.
    fn note_memory_pressure(&self, used: usize, limit: usize) {
        if limit == 0 {
            return;
        }
        let at_pressure = (used as u128) * 100 >= (limit as u128) * 80;
        let mut state = lock_state(&self.state);
        if !at_pressure {
            state.memory_pressure_warned = false;
            return;
        }
        if state.memory_pressure_warned {
            return;
        }
        state.memory_pressure_warned = true;
        drop(state);
        crate::ext_diag!(
            "pir-ext: JS heap at {} MB of the {} MB memory limit ({}%); raise `extensionLimits.memoryMb` in ~/.pi/agent/settings.json or set PI_EXT_MEMORY_MB",
            used / (1024 * 1024),
            limit / (1024 * 1024),
            (used as u128) * 100 / (limit as u128)
        );
    }

    /// Rewrite an extension-reported error whose text reports heap exhaustion
    /// into the actionable limit text (no-op when no cap is configured or the
    /// text is unrelated).
    pub(super) async fn decorate_memory_error(&self, text: String) -> String {
        let Some(message) = self.memory_limit_error() else {
            return text;
        };
        if !ExtensionLimits::looks_like_memory_exhaustion(&text) {
            return text;
        }
        self.sample_memory_pressure().await;
        message
    }

    /// Arm the QuickJS interrupt deadline for the upcoming timed call.
    /// Returns the owning token to pass to [`Self::disarm_deadline`] (P1-4:
    /// overlapping timed calls each keep their own deadline entry).
    pub(super) fn arm_deadline(&self) -> u64 {
        self.arm_deadline_for(self.call_timeout())
    }

    /// Arm the interrupt deadline with an explicit timeout (renderer calls
    /// use a shorter, UI-bound budget than the general call timeout).
    pub(super) fn arm_deadline_for(&self, timeout: Duration) -> u64 {
        let token = self
            .next_deadline_token
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        lock_state(&self.deadlines).insert(token, std::time::Instant::now() + timeout);
        token
    }

    /// Clear THIS call's interrupt deadline; other in-flight timed calls
    /// keep theirs (P1-4).
    pub(super) fn disarm_deadline(&self, token: u64) {
        lock_state(&self.deadlines).remove(&token);
    }

    /// Recover the runtime after an interrupt: an aborted job (or the aborted
    /// promise chain) leaves the uncatchable "interrupted" exception pending
    /// on the QuickJS context, which would make every subsequent JS call fail
    /// with `Error::Exception`. Clear it and drain the pending-job queue
    /// (bounded) so the runtime stays usable; the broken continuations are
    /// discarded.
    pub(super) async fn recover_after_interrupt(&self) {
        recover_interrupted(&self.ctx, &self.js_runtime).await;
    }

    /// Clear a background-job interrupt left behind by the drive loop (the
    /// preemption rule or the background budget), before the next command
    /// runs its JS. Returns whether a recovery actually ran. The drain itself
    /// is bounded by a fresh deadline so a queue full of hostile
    /// continuations cannot re-block the actor.
    pub(crate) async fn recover_background_interrupt(&self) -> bool {
        let pending = {
            let state = lock_state(&self.state);
            state
                .background_interrupted
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        };
        if !pending {
            return false;
        }
        let token = self.arm_deadline_for(BACKGROUND_RECOVERY_TIMEOUT);
        self.recover_after_interrupt().await;
        self.disarm_deadline(token);
        true
    }

    /// Drop the queued side effects of a dispatch the caller abandoned, and
    /// clear its abandoned marker.
    pub(crate) fn purge_abandoned_dispatch(&self, epoch: u64) {
        lock_state(&self.state).purge_abandoned_dispatch(epoch);
    }

    pub(super) async fn refresh_registry(&self) -> anyhow::Result<()> {
        // Cheap early-out: when no JS registration moved the registry since
        // the last publication, the full export (JSON.stringify of every tool
        // schema, command, shortcut and renderer, then a Rust re-parse) would
        // re-install exactly the snapshot already in `RuntimeState`. This is
        // the common case on the hot dispatch path: `emit` runs after every
        // event, and per-delta `message_update` emissions carry no
        // registrations at all.
        //
        // Only the handler counts can still be stale with an unchanged
        // registry (a `pi.on(...)` executed by a handler DURING this
        // dispatch, e.g. inside `session_start`): those publish through the
        // counts-only channel, which is the same publication the
        // non-dispatch entry points use. The "registration during dispatch is
        // visible before `emit` returns" invariant holds either way — a
        // `register*` call bumps `registry_gen`, a `pi.on` call bumps
        // `handlers_gen`, and both are read here after the dispatch returned.
        // The guard's scope ends HERE (`then` returns the only value that
        // escapes it), so no lock is held across the `await` below.
        let clean_registry_handlers_stale = {
            let state = lock_state(&self.state);
            (state.registry_gen == state.registry_snapshot_gen)
                .then(|| state.handlers_gen != state.handlers_snapshot_gen)
        };
        if let Some(handlers_stale) = clean_registry_handlers_stale {
            if handlers_stale {
                self.refresh_handler_counts().await?;
            }
            return Ok(());
        }
        // Read the dirty generations BEFORE the export: the counts/defs in
        // this snapshot are computed after these reads, so each
        // `(snapshot, gen)` pair is internally consistent — a mutation landing
        // after a read leaves `*_gen > *_snapshot_gen` and the next refresh
        // re-exports (conservative), while reading a generation *after* the
        // export would let that window go unnoticed.
        let (handlers_gen_before, registry_gen_before) = {
            let state = lock_state(&self.state);
            (state.handlers_gen, state.registry_gen)
        };
        let json = self.invoke_0("__pir_exportRegistryJson").await?;
        #[derive(Deserialize)]
        struct Registry {
            #[serde(default)]
            tools: Vec<ToolDef>,
            /// Per-tool wire extras the typed `ToolDef` does not model
            /// (`reachesTools` marker, `namespace.aliases`); parked in
            /// `RuntimeState::tool_extras` and merged back by
            /// `getAllToolsJson`.
            #[serde(default, rename = "toolExtras")]
            tool_extras: HashMap<String, JsonValue>,
            #[serde(default)]
            commands: Vec<CommandDef>,
            #[serde(default)]
            shortcuts: Vec<ShortcutDef>,
            #[serde(default)]
            handlers: HashMap<String, usize>,
            #[serde(default)]
            renderers: JsonValue,
        }
        let registry: Registry = serde_json::from_str(&json)?;
        let mut state = lock_state(&self.state);
        state.tool_defs = registry.tools;
        state.tool_extras = registry.tool_extras;
        state.command_defs = registry.commands;
        state.shortcut_defs = registry.shortcuts;
        state.handler_counts = registry.handlers;
        state.handlers_snapshot_gen = handlers_gen_before;
        state.registry_snapshot_gen = registry_gen_before;
        state.renderer_defs = registry.renderers;
        // The export COMMIT is itself a definition write: it is the moment the
        // JS-registry classes (tools, commands, shortcuts) actually land in
        // the state the plugin host's fingerprints read. `registryDirty`
        // already bumped `defs_gen` at the JS registration, but the host's
        // gate can run between that bump and this commit (a background JS tool
        // registering while the session's MCP wait hook re-checks the gate):
        // it would read the still-stale defs, find the fingerprints unchanged,
        // and re-latch the already-moved generation — leaving a later commit
        // of this export invisible to the O(1) half. Bumping at the commit
        // closes that window; the move can only cost the host one re-read
        // (the fingerprint comparison stays the rebuild decision).
        state.defs_gen = state.defs_gen.wrapping_add(1);
        Ok(())
    }

    pub(super) async fn invoke_0(&self, name: &str) -> rquickjs::Result<String> {
        self.ctx
            .async_with(async |ctx| {
                let f: rquickjs::Function = ctx.globals().get(name)?;
                let result: MaybePromise = f.call(())?;
                result.into_future::<String>().await
            })
            .await
    }

    pub(super) async fn invoke_str1(&self, name: &str, a: String) -> rquickjs::Result<String> {
        self.ctx
            .async_with(async |ctx| {
                let f: rquickjs::Function = ctx.globals().get(name)?;
                let result: MaybePromise = f.call((a,))?;
                result.into_future::<String>().await
            })
            .await
    }

    pub(super) async fn invoke_str2(
        &self,
        name: &str,
        a: String,
        b: String,
    ) -> rquickjs::Result<String> {
        self.ctx
            .async_with(async |ctx| {
                let f: rquickjs::Function = ctx.globals().get(name)?;
                let result: MaybePromise = f.call((a, b))?;
                result.into_future::<String>().await
            })
            .await
    }

    pub(super) async fn invoke_str3(
        &self,
        name: &str,
        a: String,
        b: String,
        c: String,
    ) -> rquickjs::Result<String> {
        self.ctx
            .async_with(async |ctx| {
                let f: rquickjs::Function = ctx.globals().get(name)?;
                let result: MaybePromise = f.call((a, b, c))?;
                result.into_future::<String>().await
            })
            .await
    }
}

/// Whether `used` bytes is at/over 95% of `limit` — the "the failed
/// allocation is why there is no room left" signal, kept at 95% (not 100%)
/// because a refused allocation is not itself counted in the heap totals.
fn heap_at_limit(used: usize, limit: usize) -> bool {
    limit > 0 && (used as u128) * 100 >= (limit as u128) * 95
}
