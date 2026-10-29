use super::*;

/// Drop every finished command task from `tasks`, logging a panic once.
///
/// `JoinSet` keeps completed tasks (and their panic payloads) until
/// `join_next`/`try_join_next` returns them. A panicking task is isolated to
/// its own command: the actor loop keeps serving the next command, and this
/// is the one place the panic becomes a diagnostic.
fn reap_finished_commands(tasks: &mut tokio::task::JoinSet<()>) {
    while let Some(result) = tasks.try_join_next() {
        if let Err(error) = result {
            crate::ext_diag!("[ext] extension actor command task panicked: {error}");
        }
    }
}

/// Actor body: construct the runtime on this thread, then serialize commands.
///
/// Runs on the dedicated `pir-ext-actor` thread. All QuickJS access happens
/// under this thread's current-thread tokio runtime. Each accepted command is
/// spawned as its own task on that runtime: commands still START in arrival
/// order, but they interleave at await points, which is what allows a JS tool
/// executing `ctx.executeTool` to re-enter the runtime for a nested call
/// while its own QuickJS execution is suspended awaiting the host promise.
/// (rquickjs releases the runtime lock while an in-flight JS future is
/// pending, so concurrent `async_with` closures interleave at job
/// granularity — single-threaded QuickJS semantics are preserved.)
pub(super) fn run_actor(
    config: ExtensionConfig,
    state: Arc<StdMutex<RuntimeState>>,
    mut rx: mpsc::Receiver<Command>,
    ready_tx: std::sync::mpsc::Sender<anyhow::Result<()>>,
) {
    let rt = Arc::new(
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(error) => {
                let _ = ready_tx.send(Err(anyhow::anyhow!(
                    "extension actor failed to start its executor: {error}"
                )));
                return;
            }
        },
    );

    let runtime = match rt.block_on(ExtensionRuntime::new_with_state(
        config.clone(),
        state.clone(),
    )) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready_tx.send(Err(anyhow::anyhow!(
                "extension runtime failed to initialize: {error}"
            )));
            return;
        }
    };
    let _ = ready_tx.send(Ok(()));

    let actor_rt = rt.clone();
    let local_set = tokio::task::LocalSet::new();
    local_set.block_on(&actor_rt, async move {
        let runtime: std::rc::Rc<StdMutex<Option<std::rc::Rc<ExtensionRuntime>>>> =
            std::rc::Rc::new(StdMutex::new(Some(std::rc::Rc::new(runtime))));
        // Keep the rquickjs executor driven for the lifetime of this runtime
        // generation (see `ExtensionRuntime::spawn_drive_loop`).
        if let Some(current) = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            current.spawn_drive_loop();
        }
        // Local tasks stay on the actor thread (rquickjs is !Send) while
        // still interleaving at await points. Tracking them in a `JoinSet`
        // lets the explicit shutdown command drain work accepted before it
        // instead of tearing the runtime down under a live command.
        let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        while let Some(command) = rx.recv().await {
            // Completed tasks are retained by a `JoinSet` until joined; in a
            // months-long session one entry per command would accumulate
            // forever. Reap them opportunistically on every mailbox turn and
            // surface a panic explicitly (the command's reply channel already
            // failed, so the caller saw an error; this names the cause in the
            // log instead of losing the payload).
            reap_finished_commands(&mut tasks);
            // Explicit shutdown is an inline control command: it must not be
            // spawned behind the very tasks it drains. No command is accepted
            // after it; the mailbox is dropped when the loop breaks.
            if let Command::Shutdown { reply } = command {
                // Commands already accepted by the mailbox were spawned and
                // may be mid-flight. Give them a bounded window to finish;
                // abort (and reap) whatever ignores the shutdown so teardown
                // cannot hang on a wedged extension.
                let drained = tokio::time::timeout(Duration::from_secs(2), async {
                    while tasks.join_next().await.is_some() {}
                })
                .await;
                if drained.is_err() {
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                }
                let result = shutdown_runtime(&runtime).await;
                let _ = reply.send(result);
                break;
            }
            let runtime = runtime.clone();
            let state = state.clone();
            let config = config.clone();
            let _ = tasks.spawn_local(async move {
                // This task owns one queued host command: count it as
                // in-flight for the background-preemption rule, and decrement
                // the handle's pending counter when the command completes
                // (BEFORE the trailing job sweep, which is background work).
                let mut flight = CommandFlight::begin(&state);
                let mut queued = QueuedCommand::begin(&state, command.counts_pending());
                // A background job interrupted by the preemption rule left its
                // uncatchable interrupt exception pending; clear it (and drain
                // the broken continuation jobs) before this command runs.
                if let Some(runtime) = current_runtime(&runtime) {
                    runtime.recover_background_interrupt().await;
                }
                match command {
                    Command::Load { paths, reply } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                let _ = reply.send(runtime.load(&paths).await);
                            }
                            None => {
                                let _ = reply
                                    .send(Err(anyhow::anyhow!("extension runtime is reloading")));
                            }
                        }
                    }
                    Command::Emit {
                        event,
                        payload,
                        epoch,
                        timeout,
                        reply,
                    } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                // This task owns the only strong reference to
                                // the Arc (the handle dropped its copy on
                                // send), so the unwrap is a plain move.
                                let mut payload = match Arc::try_unwrap(payload) {
                                    Ok(owned) => owned,
                                    Err(arc) => (*arc).clone(),
                                };
                                // The epoch tags every side effect this
                                // dispatch queues, so a host-side timeout can
                                // discard a late completion instead of letting
                                // it reach a later run. The caller's budget is
                                // enforced here too: the QuickJS interrupt
                                // deadline aborts a handler awaiting a slow
                                // host future instead of leaving the actor
                                // busy after the caller gave up.
                                let result = crate::extensions::runtime::with_dispatch_epoch(
                                    epoch,
                                    runtime.emit_with_budget(&event, &mut payload, timeout, epoch),
                                )
                                .await;
                                runtime.purge_abandoned_dispatch(epoch);
                                let _ = reply.send(result.map(|outcome| (outcome, payload)));
                            }
                            None => {
                                let _ = reply
                                    .send(Err(anyhow::anyhow!("extension runtime is reloading")));
                            }
                        }
                    }
                    Command::EmitReadOnly {
                        event,
                        payload,
                        epoch,
                        reply,
                    } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                // The payload was serialized on the caller's
                                // thread (see the command's docs): only the
                                // string crosses the mailbox, and the JS side
                                // builds no reply to send back. The epoch is
                                // the caller's, so its timeout can abandon the
                                // dispatch (see `Command::EmitReadOnly`).
                                let result = crate::extensions::runtime::with_dispatch_epoch(
                                    epoch,
                                    runtime.emit_read_only(&event, payload, epoch),
                                )
                                .await;
                                runtime.purge_abandoned_dispatch(epoch);
                                let _ = reply.send(result);
                            }
                            None => {
                                let _ = reply
                                    .send(Err(anyhow::anyhow!("extension runtime is reloading")));
                            }
                        }
                    }
                    Command::CallTool {
                        name,
                        call_id,
                        params,
                        reply,
                    } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                let _ =
                                    reply.send(runtime.call_tool(&name, &call_id, &params).await);
                            }
                            None => {
                                let _ = reply
                                    .send(Err(anyhow::anyhow!("extension runtime is reloading")));
                            }
                        }
                    }
                    Command::PrepareLoadout {
                        name,
                        loadout,
                        reply,
                    } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                let _ =
                                    reply.send(runtime.call_prepare_loadout(&name, &loadout).await);
                            }
                            None => {
                                let _ =
                                    reply.send(Err("extension runtime is reloading".to_owned()));
                            }
                        }
                    }
                    Command::CallSessionCallback { token, reply } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                let result = runtime.call_session_callback(token).await;
                                let _ = reply.send(result.map_err(|error| error.to_string()));
                            }
                            None => {
                                let _ =
                                    reply.send(Err("extension runtime is reloading".to_owned()));
                            }
                        }
                    }
                    Command::CallBashOperations {
                        token,
                        request,
                        reply,
                    } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                let _ = reply
                                    .send(runtime.call_bash_operations(&token, &request).await);
                            }
                            None => {
                                let _ =
                                    reply.send(Err("extension runtime is reloading".to_owned()));
                            }
                        }
                    }
                    Command::RunSlash { name, args, reply } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                let _ = reply.send(runtime.dispatch_command(&name, &args).await);
                            }
                            None => {
                                let _ = reply
                                    .send(Err(anyhow::anyhow!("extension runtime is reloading")));
                            }
                        }
                    }
                    Command::InvokeShortcut { key, reply } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                let _ = reply.send(runtime.invoke_shortcut(&key).await);
                            }
                            None => {
                                let _ = reply
                                    .send(Err(anyhow::anyhow!("extension runtime is reloading")));
                            }
                        }
                    }
                    Command::Reload { reply } => {
                        let _ = reply.send(reload_runtime(&config, &state, &runtime).await);
                    }
                    Command::ClearAbort { reply } => {
                        if let Some(runtime) = current_runtime(&runtime) {
                            runtime.clear_abort().await;
                        }
                        let _ = reply.send(());
                    }
                    Command::Eval { code, reply } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                let result = runtime
                                    .eval_json(&code)
                                    .await
                                    .map_err(|error| anyhow::anyhow!("{error}"));
                                let _ = reply.send(result);
                            }
                            None => {
                                let _ = reply
                                    .send(Err(anyhow::anyhow!("extension runtime is reloading")));
                            }
                        }
                    }
                    Command::InvokeRenderer {
                        request,
                        timeout,
                        reply,
                    } => {
                        let runtime = current_runtime(&runtime);
                        let result = match runtime {
                            Some(runtime) => runtime
                                .invoke_renderer(&request, timeout)
                                .await
                                .map_err(|error| anyhow::anyhow!("{error}")),
                            None => Err(anyhow::anyhow!("extension runtime is reloading")),
                        };
                        let _ = reply.send(result);
                    }
                    Command::GetArgCompletions {
                        command,
                        prefix,
                        reply,
                    } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                let _ = reply
                                    .send(runtime.argument_completions(&command, &prefix).await);
                            }
                            None => {
                                let _ = reply
                                    .send(Err(anyhow::anyhow!("extension runtime is reloading")));
                            }
                        }
                    }
                    Command::GetSuggestions {
                        context_json,
                        reply,
                    } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                let _ =
                                    reply.send(runtime.extension_suggestions(&context_json).await);
                            }
                            None => {
                                let _ = reply
                                    .send(Err(anyhow::anyhow!("extension runtime is reloading")));
                            }
                        }
                    }
                    Command::CallHookStream {
                        hook_key,
                        payload,
                        events,
                    } => {
                        let runtime = current_runtime(&runtime);
                        match runtime {
                            Some(runtime) => {
                                // Events (and the fail-closed terminal) ride
                                // the `events` channel; there is no reply
                                // oneshot. Dropping the receiver stops the
                                // pull loop.
                                runtime.call_hook_stream(&hook_key, &payload, events).await;
                            }
                            None => {
                                let _ = events
                                    .send(crate::extensions::runtime::HookStreamItem::Failed(
                                        "extension runtime is reloading".to_owned(),
                                    ))
                                    .await;
                            }
                        }
                    }
                    Command::Shutdown { reply } => {
                        // The receive loop intercepts `Shutdown` before any
                        // command task is spawned; this arm only satisfies
                        // exhaustiveness.
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "shutdown command cannot be spawned as a command task"
                        )));
                    }
                }
                // The command itself is complete: drop its in-flight count so
                // the trailing sweep is classified as BACKGROUND JS (bounded
                // by the drive loop's budget and preemptible by the next
                // queued command), and release its pending slot.
                flight.finish();
                queued.finish();
                // Sweep queued JS continuation jobs: resolving a promise from
                // a spawned host future (nested `ctx.executeTool`, dialogs)
                // can leave the awaiting JS continuation queued when the
                // resolving command task finished first; drive the job queue
                // so suspended callers resume.
                if let Some(runtime) = current_runtime(&runtime) {
                    runtime.drain_pending_jobs().await;
                }
            });
        }
        // Stale-instance protocol once the mailbox closes: fire
        // `session_shutdown {reason: "quit"}` (best-effort), then every
        // captured `pi`/`ctx` object throws instead of silently working.
        // An explicit `Shutdown` already took the runtime out above, so this
        // remains exactly-once for both the drop and explicit paths.
        if let Some(runtime) = current_runtime(&runtime) {
            let mut payload = serde_json::json!({ "type": "session_shutdown", "reason": "quit" });
            let _ = runtime.emit("session_shutdown", &mut payload).await;
            runtime.invalidate();
        }
    });
}

/// Tear one runtime generation down on the actor thread: emit the quit
/// lifecycle event, invalidate every captured `pi`/`ctx`, then take the
/// runtime out of its slot and drop it. Removing it also makes the
/// mailbox-close tail a no-op, so `session_shutdown` is emitted exactly once.
async fn shutdown_runtime(
    slot: &StdMutex<Option<std::rc::Rc<ExtensionRuntime>>>,
) -> anyhow::Result<()> {
    let Some(runtime) = current_runtime(slot) else {
        return Ok(());
    };
    let mut payload = serde_json::json!({ "type": "session_shutdown", "reason": "quit" });
    let result = runtime.emit("session_shutdown", &mut payload).await;
    runtime.invalidate();
    // Release the generation's child/worker/fetch/server/net handles BEFORE
    // the `Rc` is dropped: a still-suspended command task holding an `Rc` can
    // defer `Drop`, and a shutdown must not leave the process table live.
    // Idempotent with `Drop`.
    runtime.release_host_resources();
    // The guard is released before the `Rc` is dropped, so no state/slot lock
    // is held while the QuickJS destructor runs on its owning thread.
    let removed = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    drop(removed);
    result.map(|_| ())
}

/// Snapshot the current runtime (empty while a reload is in flight).
fn current_runtime(
    runtime: &StdMutex<Option<std::rc::Rc<ExtensionRuntime>>>,
) -> Option<std::rc::Rc<ExtensionRuntime>> {
    runtime
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Counts one command task as executing for the background-preemption rule:
/// while this is held, a background JS job is never interrupted on behalf of a
/// queued command (the executing dispatch owns its own armed deadline). The
/// count is dropped explicitly BEFORE the trailing job sweep so the sweep
/// counts as background work.
struct CommandFlight {
    state: Arc<StdMutex<RuntimeState>>,
    active: bool,
}

impl CommandFlight {
    fn begin(state: &Arc<StdMutex<RuntimeState>>) -> Self {
        crate::extensions::runtime::lock_state(state)
            .commands_in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self {
            state: Arc::clone(state),
            active: true,
        }
    }

    fn finish(&mut self) {
        if self.active {
            self.active = false;
            crate::extensions::runtime::lock_state(&self.state)
                .commands_in_flight
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

impl Drop for CommandFlight {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Releases one slot of the handle's `pending_commands` counter when the
/// command completes (the handle increments it before sending, for the
/// commands [`Command::counts_pending`] reports).
struct QueuedCommand {
    state: Arc<StdMutex<RuntimeState>>,
    counted: bool,
    active: bool,
}

impl QueuedCommand {
    fn begin(state: &Arc<StdMutex<RuntimeState>>, counted: bool) -> Self {
        Self {
            state: Arc::clone(state),
            counted,
            active: true,
        }
    }

    fn finish(&mut self) {
        if self.active {
            self.active = false;
            if !self.counted {
                return;
            }
            let state = crate::extensions::runtime::lock_state(&self.state);
            let _ = state.pending_commands.try_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |pending| Some(pending.saturating_sub(1)),
            );
        }
    }
}

impl Drop for QueuedCommand {
    fn drop(&mut self) {
        self.finish();
    }
}

/// `/reload` implementation (actor thread): guard against double reloads,
/// build the fresh runtime first, then tear down the old one, clear
/// registries, re-load the last paths, and fire the reload lifecycle events.
/// On construction failure the previous runtime is kept fully usable —
/// `session_shutdown` is only emitted and the old instance only invalidated
/// once its replacement exists.
async fn reload_runtime(
    config: &ExtensionConfig,
    state: &Arc<StdMutex<RuntimeState>>,
    runtime: &StdMutex<Option<std::rc::Rc<ExtensionRuntime>>>,
) -> anyhow::Result<LoadResult> {
    {
        let mut guard = crate::extensions::runtime::lock_state(state);
        if guard.reloading {
            return Err(anyhow::anyhow!("a reload is already in progress"));
        }
        guard.reloading = true;
    }

    let result = perform_reload(config, state, runtime).await;

    crate::extensions::runtime::lock_state(state).reloading = false;
    result
}

async fn perform_reload(
    config: &ExtensionConfig,
    state: &Arc<StdMutex<RuntimeState>>,
    runtime: &StdMutex<Option<std::rc::Rc<ExtensionRuntime>>>,
) -> anyhow::Result<LoadResult> {
    let paths = crate::extensions::runtime::lock_state(state)
        .load_paths
        .clone();
    if paths.is_empty() {
        return Err(anyhow::anyhow!("no extensions have been loaded yet"));
    }

    // A reload rebuilds the one shared `RuntimeState` in place while old-
    // generation JS may still be executing on this actor (a suspended
    // command task, a nested tool call, or a background continuation). The
    // state has no per-runtime generation marker for invalidation, so the
    // reload edge is serialized instead: refuse while another command is
    // executing. This command itself holds one `CommandFlight` slot, so any
    // count above one means another command task is still in flight.
    // Background continuations left by an already-finished command are not
    // covered by this count; they are contained by the generation-scoped
    // extension ids and fd table (`ExtensionRuntime::new_with_state` /
    // `Drop`), which keep a stale generation from matching the fresh
    // runtime's registrations or releasing its descriptors.
    let in_flight = crate::extensions::runtime::lock_state(state)
        .commands_in_flight
        .load(std::sync::atomic::Ordering::SeqCst);
    if in_flight > 1 {
        return Err(anyhow::anyhow!(
            "cannot reload extensions while {} command(s) are still executing; \
             wait for the current run to finish and retry /reload",
            in_flight - 1
        ));
    }

    // Take the old runtime out so concurrent commands report "reloading",
    // but KEEP it alive across construction: a failed rebuild must leave the
    // previous instance usable (the documented `ExtensionHandle::reload`
    // contract). It is invalidated and dropped only after the fresh runtime
    // exists.
    let old = runtime
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();

    // Build the fresh runtime before touching the old instance or the shared
    // registries, so every failure path below can restore the old runtime
    // exactly as it was.
    let fresh = match ExtensionRuntime::new_with_state(config.clone(), state.clone()).await {
        Ok(fresh) => fresh,
        Err(error) => {
            *runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = old;
            return Err(anyhow::anyhow!(
                "reload failed to rebuild the runtime: {error}"
            ));
        }
    };
    let fresh = std::rc::Rc::new(fresh);

    // The replacement exists: now tear the old generation down. In-flight
    // tasks that still hold an `Rc` keep it alive until they finish; its
    // `Drop` releases only the descriptors tagged with its own generation.
    if let Some(old) = old.as_ref() {
        let mut payload = serde_json::json!({ "type": "session_shutdown", "reason": "reload" });
        let _ = old.emit("session_shutdown", &mut payload).await;
        old.invalidate();
        // Release the old generation's children/workers/fetches/servers/
        // watchers before the fresh load starts; otherwise both generations'
        // resource tables coexist during reload on memory-constrained hosts.
        old.release_host_resources();
    }

    // Extension-owned registries reset before the fresh runtime loads; the
    // extension factories re-register everything (tools, commands, flags,
    // shortcuts, handlers, providers) exactly once.
    //
    // The package-manifest discovery cache is process-wide and fingerprint-
    // validated, but a reload is the explicit configuration-change point: drop
    // it so the rebuilt runtime re-reads every manifest it needs.
    crate::extensions::loader::npm_resolve::clear_manifest_cache();
    //
    // The model catalog is deliberately NOT part of that reset: it lives on
    // the shared `RuntimeState` and was pushed by the session host (pre-load
    // for child/factory paths, then refreshed before every prompt), so a
    // reload's factories still resolve models against the live catalog.
    crate::extensions::runtime::lock_state(state).clear_extension_registries();

    // The fresh runtime generation needs its own executor drive loop.
    fresh.spawn_drive_loop();

    let result = fresh.load(&paths).await;
    // Reload lifecycle on the new runtime regardless of per-extension load
    // errors (failing extensions are isolated entries).
    let mut payload = serde_json::json!({ "type": "session_start", "reason": "reload" });
    if let Err(error) = fresh.emit("session_start", &mut payload).await {
        crate::ext_diag!("[ext] session_start (reload) dispatch failed: {error}");
    }
    let mut payload = serde_json::json!({ "type": "resources_discover", "reason": "reload" });
    let _ = fresh.emit("resources_discover", &mut payload).await;
    *runtime
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fresh);
    result
}
