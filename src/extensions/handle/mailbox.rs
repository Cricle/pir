use super::*;

/// How long [`ExtensionHandle::shutdown`] waits for the actor to report that
/// the JS `session_shutdown` dispatch finished and the runtime was dropped.
/// Bounds teardown even when an extension handler wedges the actor.
const SHUTDOWN_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

impl ExtensionHandle {
    /// Create a runtime on a dedicated actor thread and return its handle.
    ///
    /// Blocks until the actor is ready (QuickJS context built + prelude
    /// injected, typically well under a second). Returns `Err` when the
    /// QuickJS runtime cannot be created; the actor thread always exits
    /// cleanly in that case.
    /// A build without the `extensions` feature has no embedded JS engine, so
    /// there is no runtime to start: the call fails with a clear reason (the
    /// host logs it) instead of running silently without extensions.
    #[cfg(not(feature = "extensions"))]
    pub fn start(config: ExtensionConfig) -> anyhow::Result<Self> {
        let _ = config;
        anyhow::bail!("this build has no JS extension runtime (the `extensions` feature is off)")
    }
    #[cfg(feature = "extensions")]
    pub fn start(mut config: ExtensionConfig) -> anyhow::Result<Self> {
        let started = crate::config::timings::start_phase();
        // Resolve the engine limits ONCE, before the actor exists: the actor
        // thread's OS stack is derived from the JS stack budget, and the
        // runtime then adopts the same value verbatim instead of reading the
        // global settings file (and the PI_EXT_* environment) a second time.
        if config.limits.is_none() {
            config.limits = Some(crate::extensions::limits::ExtensionLimits::resolve(None));
        }
        let state = Arc::new(StdMutex::new(RuntimeState::new(config.clone())));
        // `ready` starts `false` and is only flipped once the actor reports a
        // usable runtime; the stashed clone lets the actor thread flip it
        // back to `false` on exit, so `is_ready` cannot report a dead actor
        // as usable.
        let ready = Arc::new(AtomicBool::new(false));
        let tx = start_actor(config, Arc::clone(&state), Arc::clone(&ready))?;
        let (
            pending_commands,
            preempt_background_at_ms,
            next_dispatch_epoch,
            abandoned,
            terminal_input_handlers,
        ) = {
            let state = crate::extensions::runtime::lock_state(&state);
            (
                Arc::clone(&state.pending_commands),
                Arc::clone(&state.preempt_background_at_ms),
                Arc::clone(&state.next_dispatch_epoch),
                Arc::clone(&state.abandoned_dispatch_epochs),
                Arc::clone(&state.terminal_input_handlers),
            )
        };
        let handle = Self {
            tx: Some(tx),
            state,
            loaded: Arc::new(StdMutex::new(Vec::new())),
            ready,
            path_plan: Arc::new(RwLock::new(Vec::new())),
            pending_commands,
            preempt_background_at_ms,
            next_dispatch_epoch,
            abandoned_dispatch_epochs: abandoned,
            terminal_input_handlers,
        };
        // One-time QuickJS/actor construction (prelude injected): the phase
        // `--only-builtin` and `--no-extensions` skip entirely.
        crate::config::timings::log_phase_once_opt(
            "ext.runtime.start",
            "ext.runtime.start",
            crate::config::timings::finish_phase(started),
        );
        Ok(handle)
    }

    /// Start a handle for a BUILTIN-ONLY extension plan: the shared
    /// `RuntimeState` and path plan the unified plugin host consumes, but no
    /// actor thread and no QuickJS runtime (no engine, no prelude).
    ///
    /// Every JS-shaped operation answers exactly like a started actor with
    /// nothing loaded — the state a builtin-only run produced before this
    /// constructor existed:
    ///
    /// - `emit` returns an empty outcome (no handlers, no errors) and leaves
    ///   the payload untouched; `emit_read_only` returns `Ok(())`;
    /// - `tool_defs`/`command_defs`/`flags`/`shortcuts`/`provider_defs`/
    ///   `mcp_server_defs`/`virtual_model_defs`/`renderer_defs` are empty (they
    ///   read the same `RuntimeState` the actor would update);
    /// - renderer/completion/suggestion queries return empty results and
    ///   `dispatch_command`/`invoke_shortcut` report the same "Unknown ..."
    ///   outcomes the empty runtime's JS produced;
    /// - `load` of an on-disk JS path is a loud error: by construction a
    ///   plan-only handle is only created when the runtime load list is empty,
    ///   so reaching it means a caller bypassed the loader contract.
    ///
    /// `is_ready`/`has_js_runtime` report the handle usable for what it owns
    /// (builtin registration, dispatch, fast-path state) with
    /// `has_js_runtime == false`.
    pub fn start_plan_only(config: ExtensionConfig) -> Self {
        let started = crate::config::timings::start_phase();
        let state = Arc::new(StdMutex::new(RuntimeState::new(config)));
        let (
            pending_commands,
            preempt_background_at_ms,
            next_dispatch_epoch,
            abandoned,
            terminal_input_handlers,
        ) = {
            let state = crate::extensions::runtime::lock_state(&state);
            (
                Arc::clone(&state.pending_commands),
                Arc::clone(&state.preempt_background_at_ms),
                Arc::clone(&state.next_dispatch_epoch),
                Arc::clone(&state.abandoned_dispatch_epochs),
                Arc::clone(&state.terminal_input_handlers),
            )
        };
        let handle = Self {
            tx: None,
            state,
            loaded: Arc::new(StdMutex::new(Vec::new())),
            // Ready for the capabilities this handle has: builtin/plugin
            // registration and the fast-path state accessors never need the
            // engine.
            ready: Arc::new(AtomicBool::new(true)),
            path_plan: Arc::new(RwLock::new(Vec::new())),
            pending_commands,
            preempt_background_at_ms,
            next_dispatch_epoch,
            abandoned_dispatch_epochs: abandoned,
            terminal_input_handlers,
        };
        // `--only-builtin` / builtin-only plans log this instead of
        // `ext.runtime.start`; no actor, engine or prelude is built.
        crate::config::timings::log_phase_once_opt(
            "ext.runtime.plan_only",
            "ext.runtime.plan_only",
            crate::config::timings::finish_phase(started),
        );
        handle
    }

    /// Whether the actor thread is alive and the runtime usable.
    ///
    /// Never read `true` before the actor reported readiness, and flipped
    /// back to `false` by the actor thread's exit guard (clean shutdown or a
    /// panic unwind), so a dead actor is never reported as usable.
    /// A plan-only handle (no actor, no QuickJS) is always ready for its
    /// surface — builtin registration and the fast-path state accessors.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// Load extensions from the given paths (files or directories).
    ///
    /// A failing extension produces an error entry inside [`LoadResult`];
    /// only transport-level failures surface as `Err`.
    ///
    /// A plan-only handle has no JS engine: an empty load list (the only list
    /// the loader can hand it) is a no-op success, while an on-disk path is a
    /// loud error — that combination means the caller bypassed the
    /// partition-before-start contract.
    pub async fn load(&self, paths: &[PathBuf]) -> anyhow::Result<LoadResult> {
        let Some(tx) = self.tx.clone() else {
            if paths.is_empty() {
                return Ok(LoadResult::default());
            }
            return Err(anyhow::anyhow!(
                "the extension handle was started without a JavaScript runtime (builtin-only plan); \
                 cannot load {} on-disk extension path(s)",
                paths.len()
            ));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_queued();
        if let Err(error) = tx
            .send(Command::Load {
                paths: paths.to_vec(),
                reply: reply_tx,
            })
            .await
        {
            self.command_cancelled();
            let _ = error;
            return Err(anyhow::anyhow!("extension actor is not running"));
        }
        let result = reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("extension actor dropped the load reply"))?;
        if let Ok(result) = &result {
            *self
                .loaded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = result.loaded.clone();
        }
        result
    }

    /// Dispatch an event to all registered handlers.
    ///
    /// Handler errors are isolated into [`EmitOutcome::errors`]. For
    /// `tool_call`, the returned payload carries handler mutations of
    /// `input`.
    pub async fn emit(&self, event: &str, payload: &mut JsonValue) -> anyhow::Result<EmitOutcome> {
        let event_timeout = self.event_timeout();
        self.emit_with_timeout(event, payload, event_timeout).await
    }

    /// [`Self::emit`] with an explicit caller-side budget.
    ///
    /// Used by the `before_agent_start` lifecycle dispatch, which runs under
    /// the shorter `extensionLimits.beforeAgentStartTimeoutMs` budget: when it
    /// elapses the caller stops waiting, the run proceeds, and the dispatch is
    /// marked ABANDONED — its reply is dropped with the receiver, and every
    /// side effect it queues after the timeout is discarded by the actor
    /// instead of leaking into a later run.
    pub async fn emit_with_timeout(
        &self,
        event: &str,
        payload: &mut JsonValue,
        event_timeout: Duration,
    ) -> anyhow::Result<EmitOutcome> {
        // Plan-only handle: an empty JS runtime (the old builtin-only state)
        // dispatches to zero handlers, so the outcome is exactly an empty
        // result plus the untouched payload. No mailbox, no engine.
        let Some(tx) = self.tx.clone() else {
            return Ok(EmitOutcome {
                result: JsonValue::Null,
                payload: if payload.is_object() {
                    payload.clone()
                } else {
                    JsonValue::Null
                },
                errors: Vec::new(),
            });
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        // Read the budget BEFORE handing the command to the actor: once it is
        // busy, taking the state lock here could block behind a state accessor
        // that the blocked turn is holding. Callers that pass their own budget
        // (before_agent_start) already read it the same way.
        let epoch = self.next_dispatch_epoch.fetch_add(1, Ordering::SeqCst);
        // Move the payload into the mailbox without a deep clone: the caller's
        // value is taken into an `Arc` (single ownership), the actor unwraps
        // it back, and the rewritten payload is moved home in the reply. The
        // exclusive `&mut` borrow means no other thread can observe the
        // temporarily-empty slot while we await.
        let mailbox_payload = Arc::new(std::mem::take(payload));
        self.command_queued();
        if let Err(send_error) = tx
            .send(Command::Emit {
                event: event.to_owned(),
                payload: mailbox_payload,
                epoch,
                timeout: event_timeout,
                reply: reply_tx,
            })
            .await
        {
            self.command_cancelled();
            // The actor is gone; a failed send hands the message back, so the
            // caller keeps its payload (same observable behavior as the old
            // clone-into-mailbox path).
            if let Command::Emit {
                payload: returned, ..
            } = send_error.0
            {
                *payload = match Arc::try_unwrap(returned) {
                    Ok(owned) => owned,
                    Err(arc) => (*arc).clone(),
                };
            }
            return Err(anyhow::anyhow!("extension actor is not running"));
        }
        let (outcome, payload_out) = match tokio::time::timeout(event_timeout, reply_rx).await {
            Ok(reply) => reply.map_err(|_| {
                // The actor accepted the command but died before replying; the
                // mailbox payload is gone with it, so restore the empty slot
                // to a defined value instead of leaving the caller's payload
                // as `Null` (the send-failure path below hands the value back;
                // this one cannot).
                *payload = JsonValue::Null;
                anyhow::anyhow!("extension actor dropped the emit reply")
            })??,
            Err(_) => {
                // Host-side enforcement of the documented per-call timeout.
                // The actor-side deadline cannot fire while the actor thread
                // is stuck in a long synchronous JS turn (a blocked sync
                // `node:*` host call or a tight directory scan): the tokio
                // timer never gets polled. This deadline runs on the caller's
                // runtime, so a stuck extension can no longer stall the
                // prompt/host indefinitely. The reply is delivered to a
                // dropped channel when the actor eventually finishes, and the
                // ABANDONED marker makes the actor drop the dispatch's late
                // side effects instead of applying them to the next run.
                {
                    let mut abandoned = self
                        .abandoned_dispatch_epochs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Bounded: an abandoned epoch is normally cleared by the
                    // handler's own `exitDispatch`; a handler torn down by an
                    // interrupt may never settle, so cap the set (oldest
                    // evicted — its notifications are already lost with it).
                    const MAX_ABANDONED: usize = 256;
                    if abandoned.len() >= MAX_ABANDONED {
                        abandoned.pop_first();
                    }
                    abandoned.insert(epoch);
                }
                self.preempt_background_at_ms.store(
                    crate::extensions::runtime::monotonic_millis(),
                    Ordering::Relaxed,
                );
                *payload = JsonValue::Null;
                return Err(anyhow::anyhow!(
                    "event `{event}` dispatch timed out after {event_timeout:?} \
                         (the extension actor is unresponsive)"
                ));
            }
        };
        *payload = payload_out;
        Ok(outcome)
    }

    /// Dispatch `before_agent_start` under its dedicated budget
    /// (`extensionLimits.beforeAgentStartTimeoutMs` /
    /// `PI_EXT_BEFORE_AGENT_START_TIMEOUT_MS`, default 10 s).
    ///
    /// The run proceeds with a warning when the budget elapses; a handler that
    /// completes later is abandoned (see [`Self::emit_with_timeout`]).
    pub async fn emit_before_agent_start(
        &self,
        payload: &mut JsonValue,
    ) -> anyhow::Result<EmitOutcome> {
        let budget = crate::extensions::runtime::lock_state(&self.state).before_agent_start_timeout;
        self.emit_with_timeout("before_agent_start", payload, budget)
            .await
    }

    /// Note one host command waiting for the actor. A background JS job (a
    /// promise continuation from a finished command) is preempted after
    /// [`crate::extensions::runtime::BACKGROUND_PREEMPT_GRACE`] so the command
    /// never waits behind extension background work; the actor releases the
    /// slot when the command completes.
    pub(super) fn command_queued(&self) {
        self.pending_commands.fetch_add(1, Ordering::SeqCst);
        let preempt_at = crate::extensions::runtime::monotonic_millis()
            + crate::extensions::runtime::BACKGROUND_PREEMPT_GRACE
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
        self.preempt_background_at_ms
            .store(preempt_at, Ordering::Relaxed);
    }

    /// Undo [`Self::command_queued`] when the command could not be sent.
    pub(super) fn command_cancelled(&self) {
        let _ = self
            .pending_commands
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |pending| {
                Some(pending.saturating_sub(1))
            });
    }

    /// The configured per-call event-dispatch budget
    /// (`extensionLimits.callTimeoutMs` / `PI_EXT_CALL_TIMEOUT_MS`, default
    /// 30 s). Read from the shared state the actor updates on reload/limits.
    fn event_timeout(&self) -> Duration {
        crate::extensions::runtime::lock_state(&self.state).call_timeout
    }

    /// Dispatch an event through the reply-free extension path: the JS
    /// handlers observe the event exactly as [`Self::emit`] would deliver it,
    /// but no reply payload is built or transferred. For an event whose
    /// handler return value the caller ignores this is pure savings; for one
    /// whose payload is a deep tree (`session_tree`) it is what keeps the
    /// return leg stack-safe, because no `serde_json` parse/clone/drop of the
    /// payload happens on either side. `payload_json` is the payload as
    /// PRE-RENDERED JSON text, serialized by the caller on its own thread.
    /// `Ok(())` when the dispatch ran to completion.
    pub async fn emit_read_only(&self, event: &str, payload_json: String) -> anyhow::Result<()> {
        // Plan-only handle: zero handlers, nothing to deliver.
        let Some(tx) = self.tx.clone() else {
            return Ok(());
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let event_timeout = self.event_timeout();
        // Allocate the dispatch epoch HERE, on the caller's side, so it can be
        // marked abandoned from this thread. The actor enforces its own
        // deadline, but that timer cannot fire while the actor is stuck in a
        // long synchronous JS turn — exactly the case the caller's tokio
        // timeout covers. Carrying the epoch with the command lets the timeout
        // below abandon it, so the wedged dispatch's late `pi.*`/`ctx.ui.*`
        // side effects are dropped instead of landing in a later run.
        let epoch = self.next_dispatch_epoch.fetch_add(1, Ordering::SeqCst);
        // Informational streaming event: not counted, so it never preempts
        // extension background work (see `Command::counts_pending`).
        if let Err(error) = tx
            .send(Command::EmitReadOnly {
                event: event.to_owned(),
                payload: payload_json,
                epoch,
                reply: reply_tx,
            })
            .await
        {
            let _ = error;
            return Err(anyhow::anyhow!("extension actor is not running"));
        }
        match tokio::time::timeout(event_timeout, reply_rx).await {
            Ok(reply) => {
                reply.map_err(|_| anyhow::anyhow!("extension actor dropped the read-only reply"))?
            }
            Err(_) => {
                // Caller-side abandonment (same rule as `emit`'s timeout
                // arm): mark the epoch so the actor drops this dispatch's
                // queued notifications/side effects, and nudge background
                // preemption so the stuck turn yields sooner.
                {
                    let mut abandoned = self
                        .abandoned_dispatch_epochs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    const MAX_ABANDONED: usize = 256;
                    if abandoned.len() >= MAX_ABANDONED {
                        abandoned.pop_first();
                    }
                    abandoned.insert(epoch);
                }
                self.preempt_background_at_ms.store(
                    crate::extensions::runtime::monotonic_millis(),
                    Ordering::Relaxed,
                );
                Err(anyhow::anyhow!(
                    "event `{event}` read-only dispatch timed out after {event_timeout:?} \
                     (the extension actor is unresponsive)"
                ))
            }
        }
    }

    /// Dispatch a per-delta streaming update (`message_update`) through the
    /// read-only extension path: the JS handlers observe the event exactly as
    /// [`Self::emit`] would deliver it, but no reply payload is built or
    /// transferred (the agent loop ignores a `message_update` handler's
    /// return, so the reply cost bought nothing). `Ok(())` when the dispatch
    /// ran to completion; `wire` is the per-delta wire JSON the provider
    /// stream's thread already serialized (see
    /// [`Command::EmitReadOnly`]), so the mailbox moves O(delta) bytes and
    /// never the streamed message itself.
    pub async fn emit_stream_update(&self, wire: String) -> anyhow::Result<()> {
        self.emit_read_only("message_update", wire).await
    }

    /// Execute a registered extension tool through its JS `execute`.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        params: &JsonValue,
    ) -> anyhow::Result<ToolResult> {
        // Plan-only handle: no JS tools can be registered, so the empty
        // runtime's own "Unknown extension tool" outcome is the answer.
        let Some(tx) = self.tx.clone() else {
            return Err(anyhow::anyhow!(
                "tool `{tool_name}` failed: Unknown extension tool: {tool_name}"
            ));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_queued();
        if let Err(error) = tx
            .send(Command::CallTool {
                name: tool_name.to_owned(),
                call_id: tool_call_id.to_owned(),
                params: params.clone(),
                reply: reply_tx,
            })
            .await
        {
            self.command_cancelled();
            let _ = error;
            return Err(anyhow::anyhow!("extension actor is not running"));
        }
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("extension actor dropped the tool reply"))?
    }

    /// Dispatch a slash command by name (first registration wins).
    ///
    /// The name `reload` falls back to the in-process loader re-run (see
    /// [`Self::reload`]) only when no extension registered a JS command with
    /// that name (P2-16); a registered `reload` command wins.
    pub async fn dispatch_command(&self, name: &str, args: &str) -> anyhow::Result<CommandOutcome> {
        let js_command_registered = self
            .command_defs()
            .iter()
            .any(|command| command.name == name);
        if name == "reload" && !js_command_registered {
            let _args = args;
            return self.reload().await.map(|_| CommandOutcome {
                ok: true,
                error: None,
            });
        }
        // Plan-only handle: no JS command can be registered; the empty
        // runtime answered the same "Unknown command" outcome.
        let Some(tx) = self.tx.clone() else {
            return Ok(CommandOutcome {
                ok: false,
                error: Some(format!("Unknown command: {name}")),
            });
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_queued();
        if let Err(error) = tx
            .send(Command::RunSlash {
                name: name.to_owned(),
                args: args.to_owned(),
                reply: reply_tx,
            })
            .await
        {
            self.command_cancelled();
            let _ = error;
            return Err(anyhow::anyhow!("extension actor is not running"));
        }
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("extension actor dropped the command reply"))?
    }

    /// Invoke a registered keyboard shortcut by key combo (first registration
    /// wins). Runs the JS handler to completion and returns its outcome;
    /// `ctx.ui.*` side effects surface as notifications.
    ///
    /// TUI wiring (integrator): enumerate bindings with
    /// [`Self::shortcuts`], match normalized key combos against extension
    /// bindings during key dispatch (extension bindings override built-ins
    /// except `restrictOverride` ones), and call this method for the hit.
    pub async fn invoke_shortcut(&self, key: &str) -> anyhow::Result<CommandOutcome> {
        // Plan-only handle: no JS shortcut can be registered; the empty
        // runtime answered the same "Unknown shortcut" outcome.
        let Some(tx) = self.tx.clone() else {
            return Ok(CommandOutcome {
                ok: false,
                error: Some(format!("Unknown shortcut: {key}")),
            });
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_queued();
        if let Err(error) = tx
            .send(Command::InvokeShortcut {
                key: key.to_owned(),
                reply: reply_tx,
            })
            .await
        {
            self.command_cancelled();
            let _ = error;
            return Err(anyhow::anyhow!("extension actor is not running"));
        }
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("extension actor dropped the shortcut reply"))?
    }

    /// `/reload`: re-run the extension loader for the currently loaded
    /// extensions in-process.
    ///
    /// Mirrors upstream reload semantics: the old runtime emits
    /// `session_shutdown {reason: "reload"}` and is invalidated, registries
    /// are cleared, a fresh QuickJS runtime is built and the same paths are
    /// re-loaded (so every extension factory re-runs exactly once — no
    /// double-load), then the new runtime emits
    /// `session_start {reason: "reload"}` and
    /// `resources_discover {reason: "reload"}`. If the new runtime cannot be
    /// constructed the previous instance is kept and the error returned.
    /// A reload while another reload is in flight returns an error instead of
    /// stacking.
    pub async fn reload(&self) -> anyhow::Result<LoadResult> {
        // Plan-only handle: there is no JS runtime to rebuild and no JS path
        // was ever loaded — the same answer the builtin-only run produced
        // before (the actor's reload guard for an empty load list).
        let Some(tx) = self.tx.clone() else {
            return Err(anyhow::anyhow!("no extensions have been loaded yet"));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_queued();
        if let Err(error) = tx.send(Command::Reload { reply: reply_tx }).await {
            self.command_cancelled();
            let _ = error;
            return Err(anyhow::anyhow!("extension actor is not running"));
        }
        let result = reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("extension actor dropped the reload reply"))?;
        if let Ok(result) = &result {
            *self
                .loaded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = result.loaded.clone();
        }
        result
    }

    /// Run one deferred `withSession` callback (`ctx.newSession` /
    /// `ctx.fork` / `ctx.switchSession` option in an extension command,
    /// upstream `ReplacedSessionContext`, extensions/types.ts:437-452).
    ///
    /// The callback is a JS function that cannot cross the JSON host-fn
    /// bridge; the prelude registered it under `token` and the recorded
    /// [`SessionCommandAction`](crate::agent_session::SessionCommandAction)
    /// carried that token here. The caller must push the replacement
    /// session's context snapshot first ([`Self::update_session_context`] /
    /// the session host's own push): the callback receives a fresh command
    /// context built from it.
    ///
    /// One-shot: the token is consumed by the invocation. An unknown token
    /// (already ran, or the runtime was reloaded) and a throwing callback are
    /// errors — never a silent success. A plan-only handle has no JS runtime,
    /// so it reports that explicitly.
    pub async fn call_session_callback(&self, token: u64) -> anyhow::Result<()> {
        let Some(tx) = self.tx.clone() else {
            return Err(anyhow::anyhow!(
                "no extension runtime is attached; the withSession callback cannot run"
            ));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_queued();
        if let Err(error) = tx
            .send(Command::CallSessionCallback {
                token,
                reply: reply_tx,
            })
            .await
        {
            self.command_cancelled();
            let _ = error;
            return Err(anyhow::anyhow!("extension actor is not running"));
        }
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("extension actor dropped the withSession callback reply"))?
            .map_err(anyhow::Error::msg)
    }

    /// Explicitly terminate the JS runtime and stop the `pir-ext-actor` thread.
    ///
    /// The actor emits `session_shutdown {reason: "quit"}` to the JS handlers,
    /// invalidates the runtime, drops the QuickJS instance on its owning
    /// thread, and exits its mailbox loop. Idempotent: a plan-only handle, a
    /// mailbox already closed by an earlier shutdown, or an actor that dies
    /// during teardown all return `Ok(())`. The enqueue and reply wait are
    /// bounded by `SHUTDOWN_REPLY_TIMEOUT`; on expiry teardown reports an
    /// error instead of hanging the caller.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        // Plan-only handle: no actor thread and no QuickJS runtime to stop.
        let Some(tx) = self.tx.clone() else {
            return Ok(());
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        // Deliberately not `command_queued`: shutdown is an inline actor
        // command, not a command task the background-preemption rule counts.
        let outcome = tokio::time::timeout(SHUTDOWN_REPLY_TIMEOUT, async move {
            if tx
                .send(Command::Shutdown { reply: reply_tx })
                .await
                .is_err()
            {
                // The actor has already exited (earlier shutdown or drop).
                return Ok(());
            }
            match reply_rx.await {
                Ok(result) => result,
                // Actor died before replying: teardown already happened or
                // the thread is gone, either way nothing is left to stop.
                Err(_) => Ok(()),
            }
        })
        .await;
        match outcome {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "extension actor did not finish shutdown within {SHUTDOWN_REPLY_TIMEOUT:?}"
            )),
        }
    }

    /// Clear the abort flag and reset JS-side abort listeners.
    pub async fn clear_abort(&self) {
        // Plan-only handle: no JS listeners exist, but the shared flag is the
        // same one the empty runtime reset.
        let Some(tx) = self.tx.clone() else {
            crate::extensions::runtime::lock_state(&self.state).tool_call_aborted = false;
            return;
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_queued();
        if tx
            .send(Command::ClearAbort { reply: reply_tx })
            .await
            .is_ok()
        {
            // Best-effort: a dead actor must not fail the run start.
            let _ = reply_rx.await;
        } else {
            self.command_cancelled();
        }
    }

    /// Evaluate a JS expression that must produce a JSON string (test probe
    /// hook; mirrors `ExtensionRuntime::eval_json` through the mailbox).
    pub async fn eval_json(&self, code: &str) -> anyhow::Result<serde_json::Value> {
        // Plan-only handle: no engine to evaluate against. Callers reach this
        // only for JS-defined hooks (none can exist on a builtin-only plan);
        // a loud error keeps that contract honest.
        let Some(tx) = self.tx.clone() else {
            return Err(anyhow::anyhow!(
                "the extension handle was started without a JavaScript runtime (builtin-only plan)"
            ));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_queued();
        if let Err(error) = tx
            .send(Command::Eval {
                code: code.to_owned(),
                reply: reply_tx,
            })
            .await
        {
            self.command_cancelled();
            let _ = error;
            return Err(anyhow::anyhow!("extension actor is not running"));
        }
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("extension actor dropped the eval reply"))?
    }

    /// Abort the in-flight extension tool call (synchronous; the JS-side
    /// `signal` observes the flag through the shared state).
    pub fn abort_current_call(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tool_call_aborted = true;
    }

    /// Push the live streaming flag so `ctx.waitForIdle()` (upstream
    /// `ExtensionCommandContext.waitForIdle`, types.ts:405-406) resolves
    /// against real run state. The session calls this at every prompt-run
    /// start (`true`) and end (`false`); the `waitForIdle` host fn awaits the
    /// notify handle while a run is in flight.
    pub fn set_session_streaming(&self, streaming: bool) {
        // Plan-only handle: no actor and no JS engine, so no `ctx.waitForIdle`
        // / `ctx.isIdle` caller can observe the cell, and no `ActorLiveness`
        // guard exists to remove it. Registering one would leak an entry keyed
        // by a recyclable RuntimeState address; skip the push so the next
        // runtime at that address starts from the fresh idle default.
        if self.tx.is_none() {
            return;
        }
        crate::extensions::handle::session_stream_push(
            crate::extensions::handle::session_stream_key(&self.state),
            streaming,
        );
    }

    /// Stale-instance protocol: all captured `pi`/`ctx` calls now throw.
    pub fn invalidate(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidated = true;
    }

    /// Snapshot of registered tools after the last load.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        crate::extensions::runtime::lock_state(&self.state)
            .tool_defs
            .clone()
    }

    /// Per-tool wire extras the typed [`ToolDef`] does not model, keyed by
    /// tool name: `reachesTools` (presence), `namespace`, and the declarative
    /// `renderShell` / `constrainedSampling` / `renderCall` / `renderResult`
    /// fields (upstream `ToolDefinition`, types.ts:590-667). Synchronous: a
    /// TUI tool block reads `renderShell` here without an actor round trip.
    pub fn tool_extra(&self, name: &str) -> Option<JsonValue> {
        crate::extensions::runtime::lock_state(&self.state)
            .tool_extras
            .get(name)
            .cloned()
    }

    /// Snapshot of the extension-registered virtual-model definitions
    /// (`pi.registerVirtualModel`).
    pub fn virtual_model_defs(&self) -> Vec<JsonValue> {
        crate::extensions::runtime::lock_state(&self.state)
            .virtual_model_defs
            .clone()
    }

    /// Remove an extension-registered virtual-model definition
    /// (`pi.unregisterVirtualModel`). Returns whether one existed.
    pub fn remove_virtual_model(&self, provider: &str, id: &str) -> bool {
        crate::extensions::runtime::lock_state(&self.state).remove_virtual_model(provider, id)
    }

    /// The physical catalog model for `provider`/`id` from the host-pushed
    /// registry snapshot (upstream `getPhysicalModel`).
    pub fn get_physical_model(&self, provider: &str, id: &str) -> Option<JsonValue> {
        crate::extensions::runtime::lock_state(&self.state).get_physical_model(provider, id)
    }

    /// Run a JS tool's `prepareLoadout` hook (upstream
    /// `ToolDefinition.prepareLoadout`): `None` when the tool has no hook or
    /// the hook returned undefined; `Err` carries a thrown error or a
    /// dispatch failure.
    pub async fn call_prepare_loadout(
        &self,
        tool_name: &str,
        loadout: &JsonValue,
    ) -> Result<Option<JsonValue>, String> {
        // Plan-only handle: no JS tool exists, so no tool has a
        // `prepareLoadout` hook — the same `None` an empty runtime returned.
        let Some(tx) = self.tx.clone() else {
            return Ok(None);
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_queued();
        if let Err(error) = tx
            .send(Command::PrepareLoadout {
                name: tool_name.to_owned(),
                loadout: loadout.clone(),
                reply: reply_tx,
            })
            .await
        {
            self.command_cancelled();
            let _ = error;
            return Err("extension actor is not running".to_owned());
        }
        reply_rx
            .await
            .map_err(|_| "extension actor dropped the prepareLoadout reply".to_owned())?
    }

    /// Execute a `user_bash` handler's exported `BashOperations.exec`
    /// (upstream `UserBashEventResult.operations`). The prelude deletes the
    /// exported closure when the call settles (success or throw), so a token
    /// is single-use.
    pub async fn call_bash_operations(
        &self,
        token: &str,
        request: &JsonValue,
    ) -> Result<JsonValue, String> {
        // Plan-only handle: no JS handler could have supplied operations.
        let Some(tx) = self.tx.clone() else {
            return Err("the extension runtime is not running".to_owned());
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_queued();
        if let Err(error) = tx
            .send(Command::CallBashOperations {
                token: token.to_owned(),
                request: request.clone(),
                reply: reply_tx,
            })
            .await
        {
            self.command_cancelled();
            let _ = error;
            return Err("extension actor is not running".to_owned());
        }
        reply_rx
            .await
            .map_err(|_| "extension actor dropped the bash operations reply".to_owned())?
    }
}

/// Spawn the `pir-ext-actor` thread (QuickJS runtime + command mailbox) and
/// block until it reports readiness.
///
/// The only caller today is [`ExtensionHandle::start`]; `start_plan_only`
/// deliberately never reaches this function (the memory this saves is exactly
/// the runtime constructed by `run_actor`). Construction of the QuickJS
/// runtime is fast, so the blocking wait keeps `start` synchronous: callers
/// cannot race a load against actor readiness.
#[cfg(feature = "extensions")]
fn start_actor(
    config: ExtensionConfig,
    state: Arc<StdMutex<RuntimeState>>,
    ready: Arc<AtomicBool>,
) -> anyhow::Result<mpsc::Sender<Command>> {
    let (tx, rx) = mpsc::channel::<Command>(64);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
    let thread_state = Arc::clone(&state);
    let actor_ready = Arc::clone(&ready);
    // The streaming cell key rides with the state Arc; the actor's exit
    // guard removes the cell so the key's address can be recycled safely.
    let stream_key = crate::extensions::handle::session_stream_key(&state);
    // The QuickJS runtime grants JS a stack budget (`limits.stack_bytes`, 16
    // MiB by default); the native frames of the engine and the host share
    // this same OS stack, so it is derived from that budget instead of a flat
    // 64 MiB per extension (address-space pressure on the constrained
    // deployment targets).
    let actor_stack_bytes = config
        .limits
        .as_ref()
        .map(crate::extensions::limits::ExtensionLimits::actor_stack_bytes)
        .unwrap_or_else(|| {
            crate::extensions::limits::ExtensionLimits::default().actor_stack_bytes()
        });
    std::thread::Builder::new()
        .name("pir-ext-actor".to_owned())
        // A stack smaller than the JS budget would let deep recursion
        // overflow the OS stack before QuickJS raises its RangeError;
        // `actor_stack_bytes` is always at least `stack_bytes` and adds
        // native headroom on top.
        .stack_size(actor_stack_bytes)
        .spawn(move || {
            // Drop guard: runs on every exit path — normal mailbox shutdown
            // or an unwind through the actor — so the flag can never stay
            // stuck at `true` behind a gone actor, and the runtime's
            // streaming cell never outlives the actor.
            let _liveness = ActorLiveness(actor_ready, stream_key);
            run_actor(config, thread_state, rx, ready_tx);
        })
        .map_err(|error| anyhow::anyhow!("failed to spawn the extension actor thread: {error}"))?;
    ready_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("extension actor thread died before becoming ready"))??;
    // The ready reply happens-after the actor built its runtime, and the
    // handle cannot exist before this point; relaxed ordering suffices for
    // the flag (a later observed `false` only ever means "exited").
    ready.store(true, Ordering::Relaxed);
    Ok(tx)
}

/// Lives on the actor thread's stack and flips the handle's `ready` flag to
/// `false` when the thread exits — normal mailbox shutdown or a panic
/// unwinding through the actor — keeping [`ExtensionHandle::is_ready`]
/// honest. It also removes the runtime's session-streaming cell so the state
/// `Arc`'s heap address can be recycled without inheriting a stale flag.
#[cfg(feature = "extensions")]
struct ActorLiveness(Arc<AtomicBool>, usize);

#[cfg(feature = "extensions")]
impl Drop for ActorLiveness {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
        crate::extensions::handle::session_stream_remove(self.1);
    }
}
