use super::*;

/// Open file-descriptor registry for the fs v2 surface (`openSync` family).
/// Lives in [`RuntimeState`]; the state itself survives `/reload`, so entries
/// carry the runtime generation that opened them. Each runtime releases only
/// its own generation's entries at teardown (`ExtensionRuntime::drop`); an
/// old runtime whose `Drop` is deferred by in-flight work can therefore
/// never close a descriptor opened by its successor.
///
/// GENERATION IS PER DESCRIPTOR, never table-global: `/reload` builds the
/// successor runtime BEFORE the predecessor is torn down, and the predecessor's
/// still-running JS may keep calling `fs.openSync` during that window. A
/// mutable "current generation" on the table would then stamp the OLD
/// runtime's later descriptors with the NEW generation — its own `Drop` would
/// leave them open forever, and the successor's teardown would close
/// descriptors it never opened out from under the still-live predecessor. So
/// every insert carries the CALLER's generation explicitly.
#[cfg_attr(not(feature = "extensions"), allow(dead_code))]
pub(crate) struct FdTable {
    next_fd: i32,
    entries: HashMap<i32, FdEntry>,
}

/// Hard cap on simultaneously open `fs.open` descriptors per runtime.
///
/// The JS side can open descriptors in a loop and never close them; without a
/// cap that exhausts the host process's OS descriptor budget (and every later
/// host operation with `EMFILE`). 256 is Node's common soft-limit scale while
/// staying far below a constrained device's process-wide descriptor budget;
/// the table is shared by every extension of one runtime, so the cap is the
/// runtime's total, not one extension's.
#[cfg_attr(not(feature = "extensions"), allow(dead_code))]
pub(crate) const MAX_OPEN_FDS: usize = 256;

/// One open descriptor: the OS handle plus the host-tracked sequential
/// offset (`fs.readSync(fd, buf, ..., null)` reads from here).
#[cfg_attr(not(feature = "extensions"), allow(dead_code))]
pub(crate) struct FdEntry {
    pub(crate) file: std::fs::File,
    /// Path text for Node-shaped error messages.
    pub(crate) path: PathBuf,
    /// Node `a`/`a+` semantics: writes append regardless of the offset.
    append: bool,
    /// Sequential offset; explicit positions never move it (Node/pread
    /// behavior), absent positions read/write from here and advance it.
    position: u64,
    /// Generation of the runtime that opened this descriptor (see
    /// [`FdTable`]).
    generation: u64,
}

#[cfg_attr(not(feature = "extensions"), allow(dead_code))]
impl FdTable {
    pub(crate) fn new() -> Self {
        Self {
            // 0/1/2 are the stdio descriptors in Node; the first host fd is 3.
            next_fd: 3,
            entries: HashMap::new(),
        }
    }

    /// Register `file` and return its descriptor. Numbers are monotonic
    /// (never reused), so a stale descriptor can only fail with EBADF.
    ///
    /// `generation` is the OPENING runtime's own key (see the type docs): the
    /// descriptor is released by that runtime's teardown and by nothing else.
    ///
    /// `Err(())` when [`MAX_OPEN_FDS`] descriptors are already open; the
    /// caller maps that to Node's `EMFILE` before the file handle is stored
    /// (the unregistered `file` drops with the caller's error path).
    pub(crate) fn insert(
        &mut self,
        file: std::fs::File,
        path: PathBuf,
        append: bool,
        generation: u64,
    ) -> Result<i32, ()> {
        if self.entries.len() >= MAX_OPEN_FDS {
            return Err(());
        }
        let fd = self.next_fd;
        self.next_fd += 1;
        self.entries.insert(
            fd,
            FdEntry {
                file,
                path,
                append,
                position: 0,
                generation,
            },
        );
        Ok(fd)
    }

    pub(crate) fn get_mut(&mut self, fd: i32) -> Option<&mut FdEntry> {
        self.entries.get_mut(&fd)
    }

    /// Close: the returned entry drops (and releases the handle) with the
    /// caller's binding.
    pub(crate) fn remove(&mut self, fd: i32) -> Option<FdEntry> {
        self.entries.remove(&fd)
    }

    /// Close every descriptor owned by `generation` (runtime teardown /
    /// `/reload`). Scoped by generation: a reload's old generation and its
    /// successor share this table, and the old `Drop` may run after the
    /// successor has already opened descriptors of its own.
    pub(crate) fn clear_generation(&mut self, generation: u64) {
        self.entries
            .retain(|_, entry| entry.generation != generation);
    }
}

#[cfg_attr(not(feature = "extensions"), allow(dead_code))]
impl FdEntry {
    /// Read up to `length` bytes; `position` absent = sequential (advances
    /// the tracked offset), explicit = a positioned read (offset untouched).
    pub(crate) fn read(
        &mut self,
        length: usize,
        position: Option<u64>,
    ) -> std::io::Result<Vec<u8>> {
        let mut buffer = vec![0u8; length];
        let at = position.unwrap_or(self.position);
        let read = positioned_read(&self.file, &mut buffer, at)?;
        buffer.truncate(read);
        if position.is_none() {
            self.position = at + read as u64;
        }
        Ok(buffer)
    }

    /// Write `data`; `position` absent = sequential. Append-mode descriptors
    /// write at EOF regardless (Node's documented `a` flag behavior).
    pub(crate) fn write(&mut self, data: &[u8], position: Option<u64>) -> std::io::Result<usize> {
        let at = if self.append {
            self.file.metadata()?.len()
        } else {
            position.unwrap_or(self.position)
        };
        positioned_write(&self.file, data, at)?;
        if self.append || position.is_none() {
            self.position = at + data.len() as u64;
        }
        Ok(data.len())
    }
}

/// Positioned read that does not move the OS file pointer (`pread` /
/// `seek_read`); the tracked offset is the only sequential cursor.
#[cfg(windows)]
#[cfg_attr(not(feature = "extensions"), allow(dead_code))]
fn positioned_read(file: &std::fs::File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
}

#[cfg(unix)]
fn positioned_read(file: &std::fs::File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, offset)
}

/// Positioned write-all (`pwrite` / a `seek_write` loop — the Windows trait
/// has no `write_all_at`).
#[cfg(windows)]
#[cfg_attr(not(feature = "extensions"), allow(dead_code))]
fn positioned_write(file: &std::fs::File, data: &[u8], offset: u64) -> std::io::Result<()> {
    let mut written = 0;
    while written < data.len() {
        let n = std::os::windows::fs::FileExt::seek_write(
            file,
            &data[written..],
            offset + written as u64,
        )?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write the whole buffer",
            ));
        }
        written += n;
    }
    Ok(())
}

#[cfg(unix)]
fn positioned_write(file: &std::fs::File, data: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, data, offset)
}

/// Lock a shared-state mutex, recovering from poisoning instead of panicking.
///
/// The extension-runtime state only holds plain data guarded by short critical
/// sections, so a poisoned lock is safe to continue from: take the guard out
/// of the `PoisonError` and carry on.
pub(crate) fn lock_state<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Recover a QuickJS runtime after an interrupt: an aborted job (or the
/// aborted promise chain) leaves the uncatchable "interrupted" exception
/// pending on the context, which would make every subsequent JS call fail
/// with `Error::Exception`. Clear it and drain the pending-job queue
/// (bounded) so the runtime stays usable; the broken continuations are
/// discarded. Shared by [`ExtensionRuntime::recover_after_interrupt`] and the
/// `codemode` sandbox (which runs its own minimal QuickJS runtime).
#[cfg(feature = "extensions")]
pub(crate) async fn recover_interrupted(
    ctx: &rquickjs::AsyncContext,
    js_runtime: &rquickjs::AsyncRuntime,
) {
    let catch = |ctx: &rquickjs::Ctx<'_>| {
        if ctx.has_exception() {
            // Drop the uncatchable interrupt error.
            let _ = ctx.catch();
        }
    };
    ctx.async_with(async |ctx| catch(&ctx)).await;
    for _ in 0..128 {
        match js_runtime.execute_pending_job().await {
            Ok(true) => continue,
            Ok(false) => break,
            // The aborted continuation throws here; clear and keep
            // draining until the queue is empty.
            Err(_) => {
                ctx.async_with(async |ctx| catch(&ctx)).await;
                continue;
            }
        }
    }
    // Final sweep: nothing must stay pending on the context.
    ctx.async_with(async |ctx| catch(&ctx)).await;
}

#[cfg_attr(not(feature = "extensions"), allow(dead_code))]
impl RuntimeState {
    pub(crate) fn new(config: ExtensionConfig) -> Self {
        Self {
            session_name: config.session_id.clone(),
            call_timeout: DEFAULT_CALL_TIMEOUT,
            hook_timeout: HOOK_CALL_TIMEOUT,
            // Replaced by `ExtensionRuntime::new_with_state` with the
            // resolved limits; the default keeps this state well-formed for
            // the data-only (no engine) configurations.
            limits: ExtensionLimits::default(),
            memory_pressure_warned: false,
            memory_pressure_last_sample: None,
            thinking_level: "off".to_string(),
            session: Arc::new(SessionContextSnapshot {
                project_trusted: true,
                ..SessionContextSnapshot::default()
            }),
            scoped_models: JsonValue::Array(Vec::new()),
            model_registry: serde_json::json!({ "models": [], "providers": [] }),
            system_prompt_options: JsonValue::Null,
            system_prompt_options_key: String::new(),
            provider_defs: Vec::new(),
            mcp_server_defs: Vec::new(),
            virtual_model_defs: Vec::new(),

            config,
            invalidated: false,
            tool_call_aborted: false,
            registered_ext_ids: std::collections::BTreeSet::new(),
            flags: Vec::new(),
            flag_values: HashMap::new(),
            active_tools_override: None,
            core_tools: Vec::new(),
            tool_defs: Vec::new(),
            tool_extras: HashMap::new(),
            all_tools_json: None,
            command_defs: Vec::new(),
            shortcut_defs: Vec::new(),
            handler_counts: HashMap::new(),
            handlers_gen: 0,
            handlers_snapshot_gen: 0,
            registry_gen: 0,
            registry_snapshot_gen: 0,
            defs_gen: 0,
            load_paths: Vec::new(),
            reloading: false,
            notifications: Vec::new(),
            settings: JsonValue::Null,
            builtin_commands: Vec::new(),
            authenticated_providers: Vec::new(),
            nested_executors: HashMap::new(),
            dialog_responses: HashMap::new(),
            dialog_notifies: HashMap::new(),
            dialog_ttl: DIALOG_TIMEOUT,
            // One shared cell per runtime: the TUI clones it (tool blocks /
            // Ctrl+O) and the JS host fns read/write it (`ctx.ui` tools
            // expansion).
            tools_expanded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            terminal_input_handlers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            renderer_defs: JsonValue::Null,
            next_dialog_id: 0,
            host_completion: None,
            editor_text: String::new(),
            theme_registry: JsonValue::Null,
            has_autocomplete_suggesters: false,
            stream_hook_bridges: HashMap::new(),
            fd_table: FdTable::new(),
            before_agent_start_timeout:
                crate::extensions::limits::DEFAULT_BEFORE_AGENT_START_TIMEOUT,
            pending_commands: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            commands_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            preempt_background_at_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            background_interrupted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            abandoned_dispatch_epochs: Arc::new(StdMutex::new(std::collections::BTreeSet::new())),
            next_dispatch_epoch: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ext_identities: HashMap::new(),
            js_dispatch_epochs: Vec::new(),
        }
    }

    /// Clear the extension-owned registries (used by `/reload` between the
    /// old runtime's teardown and the fresh runtime's load). Host-pushed
    /// state (session snapshot, model registry, `core_tools`) survives; the
    /// abort flag is preserved.
    ///
    /// `invalidated` is one shared bool, so it cannot distinguish runtime
    /// generations by itself and is reset here for the fresh runtime. Stale
    /// generations are still rejected through the registry: every
    /// `ExtensionRuntime` allocates its extension ids from a process-wide
    /// disjoint range (see `NEXT_EXT_ID_BASE` in `runtime.rs`), so the
    /// cleared-and-repopulated `registered_ext_ids` can never re-admit an id
    /// captured by an older generation — `isActive` keeps throwing for old
    /// `pi` objects. `/reload` additionally refuses to run while another
    /// command is executing (see `perform_reload`), and generation-scoped fd
    /// ownership covers the background-continuation window.
    pub(crate) fn clear_extension_registries(&mut self) {
        self.invalidated = false;
        self.flags.clear();
        self.flag_values.clear();
        self.active_tools_override = None;
        self.tool_defs.clear();
        self.tool_extras.clear();
        // The memoized array embeds the extension defs just cleared; the
        // fingerprint would already miss, but dropping it here keeps the
        // invalidation explicit at the clear site.
        self.all_tools_json = None;
        self.command_defs.clear();
        self.shortcut_defs.clear();
        self.handler_counts.clear();
        // The fresh runtime starts with a new prelude whose
        // `state.terminalInputHandlers` is empty; clear the shared fast-path
        // count too so `/reload` does not leave the TUI dispatching input to a
        // runtime with no raw-input handler.
        self.terminal_input_handlers
            .store(0, std::sync::atomic::Ordering::Release);
        self.provider_defs.clear();
        self.mcp_server_defs.clear();
        self.virtual_model_defs.clear();

        self.registered_ext_ids.clear();
        self.ext_identities.clear();
        self.js_dispatch_epochs.clear();
        self.renderer_defs = JsonValue::Null;
        self.nested_executors.clear();
        // The cleared lists are exactly the classes the plugin host's gate
        // fingerprints, so the clear is a definition write like any other: move
        // `defs_gen` (the reload's fresh load re-commits through
        // `refresh_registry`, which bumps again — a second move only costs the
        // host one re-read, never a missed change).
        self.defs_gen = self.defs_gen.wrapping_add(1);
    }

    /// Hard cap for the pending-notification queue (a tool streaming thousands
    /// of partial updates, or a `ctx.ui.notify`/`setStatus`/`setWidget` flood,
    /// while no host drains must not grow memory unboundedly). Applies to
    /// EVERY [`Notification`] variant: above the cap the queue coalesces
    /// latest-wins state slots (`ToolUpdate` per tool call, `Status` per key,
    /// `Widget` per id, singleton UI slots) and drops the oldest
    /// fire-and-forget toasts before falling back to the oldest entries.
    pub(crate) const NOTIFICATION_QUEUE_CAP: usize = 1024;

    /// Slots freed by one [`Self::enforce_notification_cap`] pass. The pass
    /// costs O(cap), so leaving a quarter of the queue free ties its frequency
    /// to the work it does (at least this many pushes happen before the next
    /// pass): eviction is amortized O(1) per push, never a scan-and-shift per
    /// delta.
    const NOTIFICATION_QUEUE_TRIM: usize = Self::NOTIFICATION_QUEUE_CAP / 4;

    pub(crate) fn push(&mut self, notification: Notification) {
        // Tag the side effect with the dispatch that produced it.
        //
        // Preferred source: the Rust-side epoch of the dispatch future that is
        // currently executing on this thread (`with_dispatch_epoch`, scoped to
        // the future, so interleaved commands never see each other's epoch).
        // Fallback: the JS-side epoch stack, which still holds an entry for a
        // handler whose dispatch was abandoned while the handler was suspended
        // — that is exactly the background continuation this rule must catch.
        // The fallback is only consulted when NO command task is executing, so
        // a tool/command running in parallel is never mistaken for a late
        // handler continuation.
        let current = super::current_dispatch_epoch();
        let epoch = if current != 0 {
            current
        } else if self
            .commands_in_flight
            .load(std::sync::atomic::Ordering::Relaxed)
            == 0
            && !self.js_dispatch_epochs.is_empty()
        {
            // A background continuation: attribute to the innermost pending
            // dispatch epoch (0 when that handler has already settled).
            self.js_dispatch_epochs.last().copied().unwrap_or(0)
        } else {
            0
        };
        self.push_tagged(epoch, notification);
    }

    /// Queue a HOST-THREAD side effect (the native `UiService` seam): never
    /// tagged with a JS dispatch epoch, even when an extension handler happens
    /// to be executing on the actor concurrently (a host-originated
    /// `ctx.ui.notify` must not be mistaken for that dispatch's work and
    /// dropped on its timeout).
    pub(crate) fn push_host(&mut self, notification: Notification) {
        self.push_tagged(0, notification);
    }

    fn push_tagged(&mut self, epoch: u64, notification: Notification) {
        if epoch != 0 {
            let abandoned = self
                .abandoned_dispatch_epochs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&epoch);
            if abandoned {
                return;
            }
        }
        if self.notifications.len() >= Self::NOTIFICATION_QUEUE_CAP {
            // At the cap: one coalescing pass makes room for this push (and
            // the next `NOTIFICATION_QUEUE_TRIM - 1` pushes).
            self.enforce_notification_cap(&notification);
        }
        self.notifications.push((epoch, notification));
    }

    /// Make room for one more notification once the queue has reached
    /// [`Self::NOTIFICATION_QUEUE_CAP`].
    ///
    /// Every variant is subject to the cap. The pass first coalesces
    /// latest-wins state slots — an incoming `ToolUpdate` supersedes the
    /// queued update for the same tool call (per-call coalescing), a
    /// `Status`/`Widget`/... supersedes the queued entry with the same key —
    /// then drops the oldest `Notify` toasts (fire-and-forget, no host
    /// action), then the oldest `ToolUpdate` progress rows (ephemeral), and
    /// only if the queue is still too large drops the oldest survivors: the
    /// cap is hard, so bounded memory wins over retaining an unbounded
    /// history.
    ///
    /// `retain` preserves the relative order of the survivors, so the TUI
    /// observes exactly the enqueue order minus the dropped entries. One
    /// O(cap) pass is paid per [`Self::NOTIFICATION_QUEUE_TRIM`] pushes
    /// (the pass leaves that many slots free), so eviction is amortized O(1)
    /// instead of scanning and shifting the whole queue per delta.
    fn enforce_notification_cap(&mut self, incoming: &Notification) {
        let len = self.notifications.len();
        let target = Self::NOTIFICATION_QUEUE_CAP - Self::NOTIFICATION_QUEUE_TRIM;
        let mut keep = vec![true; len];

        // `incoming` is the newest entry: a queued state slot it targets is
        // stale and must not win over it.
        let mut seen: std::collections::HashSet<(u8, String)> = std::collections::HashSet::new();
        if let Some((slot, key)) = notification_state_slot(incoming) {
            seen.insert((slot, key.to_owned()));
        }
        // Newest-first: the first occurrence of a slot is the value that
        // survives; any older entry for the same slot is superseded.
        for (index, (_, queued)) in self.notifications.iter().enumerate().rev() {
            if let Some((slot, key)) = notification_state_slot(queued) {
                if !seen.insert((slot, key.to_owned())) {
                    keep[index] = false;
                }
            }
        }

        let kept = keep.iter().filter(|keep_entry| **keep_entry).count();
        let mut excess = kept.saturating_sub(target);

        // Fire-and-forget toasts first: dropping the oldest notices keeps the
        // user-visible state and every actionable side effect intact.
        if excess > 0 {
            for (index, (_, queued)) in self.notifications.iter().enumerate() {
                if excess == 0 {
                    break;
                }
                if keep[index] && matches!(queued, Notification::Notify { .. }) {
                    keep[index] = false;
                    excess -= 1;
                }
            }
        }

        // Then ephemeral tool progress: a dropped update only loses one
        // frame of a streaming block, while state/actionable entries carry
        // the values the host applies.
        if excess > 0 {
            for (index, (_, queued)) in self.notifications.iter().enumerate() {
                if excess == 0 {
                    break;
                }
                if keep[index] && matches!(queued, Notification::ToolUpdate { .. }) {
                    keep[index] = false;
                    excess -= 1;
                }
            }
        }

        // Hard cap: nothing droppable left, so the oldest survivors go.
        if excess > 0 {
            for (index, _) in self.notifications.iter().enumerate() {
                if excess == 0 {
                    break;
                }
                if keep[index] {
                    keep[index] = false;
                    excess -= 1;
                }
            }
        }

        let mut index = 0;
        self.notifications.retain(|_| {
            let keep_entry = keep[index];
            index += 1;
            keep_entry
        });
    }

    /// Sweep the side effects of an ABANDONED dispatch once its (interrupted)
    /// JS call returns. A normal dispatch is a no-op. The abandoned marker
    /// and the JS epoch-stack entry are deliberately KEPT: an interrupt-torn
    /// handler may resume from a pending host future later, and the marker is
    /// what makes that late continuation's side effects drop. The entry and
    /// the marker are removed by the handler's own `exitDispatch` (its JS
    /// `finally`); both collections are bounded, so a continuation that never
    /// resumes cannot grow them.
    pub(crate) fn purge_abandoned_dispatch(&mut self, epoch: u64) {
        if epoch == 0 {
            return;
        }
        let is_abandoned = self
            .abandoned_dispatch_epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&epoch);
        if is_abandoned {
            // Notifications queued before the abandonment was observed.
            self.notifications.retain(|(queued, _)| *queued != epoch);
        }
    }

    /// The JS handler for `epoch` settled (`exitDispatch`): drop its JS epoch
    /// entry and its abandoned marker (when the caller had given up on it).
    pub(crate) fn exit_js_dispatch(&mut self, epoch: u64) {
        if epoch == 0 {
            return;
        }
        self.js_dispatch_epochs.retain(|queued| *queued != epoch);
        self.abandoned_dispatch_epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&epoch);
    }

    /// Remove notifications whose dispatch epoch is already abandoned.
    /// Called by the drain paths (the producer normally drops them in
    /// [`Self::push`]; an entry queued between the timeout and the abandon
    /// marker is caught here).
    pub(crate) fn filter_abandoned_notifications(&mut self) {
        let abandoned = self
            .abandoned_dispatch_epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if abandoned.is_empty() {
            return;
        }
        self.notifications
            .retain(|(epoch, _)| *epoch == 0 || !abandoned.contains(epoch));
    }

    /// Remove dialog replies and waiter registrations past their own wait
    /// (P2-9: entries for dead waiters must not accumulate unboundedly).
    /// Each entry carries the ttl it was created with — a request-level
    /// `timeout` for a live waiter, the global dialog TTL otherwise — so a
    /// short global TTL never evicts a waiter that is still inside its own
    /// longer request timeout. Called when a reply arrives and when a waiter
    /// registers.
    pub(crate) fn sweep_expired_dialog_state(&mut self) {
        self.dialog_responses
            .retain(|_, (_, inserted_at, ttl)| inserted_at.elapsed() < *ttl);
        self.dialog_notifies
            .retain(|_, (_, created_at, ttl)| created_at.elapsed() < *ttl);
    }

    /// Current value of a registered flag (CLI value > default); `None` when
    /// the flag was never registered by any extension.
    pub(crate) fn flag_value(&self, name: &str) -> Option<FlagValue> {
        if !self.flags.iter().any(|f| f.name == name) {
            return None;
        }
        self.flag_values
            .get(name)
            .cloned()
            .or_else(|| self.flags.iter().find(|f| f.name == name)?.default.clone())
    }

    /// Apply a CLI-provided value to a registered flag.
    pub(crate) fn set_flag_value(&mut self, name: &str, value: FlagValue) -> Result<(), String> {
        let flag_type = self
            .flags
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| format!("flag `{name}` was not registered by any extension"))?
            .flag_type
            .clone();
        match (&value, flag_type.as_str()) {
            (FlagValue::Bool(_), "boolean") | (FlagValue::Str(_), "string") => {}
            _ => return Err(format!("flag `{name}` expects type `{flag_type}`")),
        }
        self.flag_values.insert(name.to_string(), value);
        Ok(())
    }

    /// Remove an extension-registered virtual-model definition
    /// (`pi.unregisterVirtualModel`). Returns whether one existed. The
    /// provider falls back to the definition's `physical.provider` (the JS
    /// registration form carries no top-level provider).
    ///
    /// Bumps [`Self::defs_gen`] on an actual removal here, at the single site
    /// that mutates the vector: both callers — the `unregisterVirtualModel`
    /// host fn and the `ExtensionHandle::remove_virtual_model` accessor — go
    /// through this helper, so a definition write can never bypass the host
    /// gate's generation.
    pub(crate) fn remove_virtual_model(&mut self, provider: &str, id: &str) -> bool {
        let provider_of = |def: &JsonValue| -> String {
            def.get("provider")
                .and_then(JsonValue::as_str)
                .or_else(|| {
                    def.get("physical")
                        .and_then(|physical| physical.get("provider"))
                        .and_then(JsonValue::as_str)
                })
                .unwrap_or_default()
                .to_owned()
        };
        let before = self.virtual_model_defs.len();
        self.virtual_model_defs.retain(|def| {
            !(provider_of(def) == provider && def.get("id").and_then(JsonValue::as_str) == Some(id))
        });
        let removed = self.virtual_model_defs.len() != before;
        if removed {
            self.defs_gen = self.defs_gen.wrapping_add(1);
        }
        removed
    }

    /// The physical catalog model for `provider`/`id` (upstream
    /// `getPhysicalModel`): the host-pushed registry snapshot filtered to
    /// non-virtual entries. `None` when unknown or virtual.
    pub(crate) fn get_physical_model(&self, provider: &str, id: &str) -> Option<JsonValue> {
        self.model_registry
            .get("models")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .find(|model| {
                model.get("provider").and_then(JsonValue::as_str) == Some(provider)
                    && model.get("id").and_then(JsonValue::as_str) == Some(id)
                    && model.get("api").and_then(JsonValue::as_str)
                        != Some(crate::ai::virtual_::VIRTUAL_MODEL_API)
            })
            .cloned()
    }
}

/// Latest-wins state slot of a notification: entries sharing a `(slot, key)`
/// set the same piece of host state, so under queue pressure only the newest
/// one has to survive. `None` marks one-shot notifications (messages, user
/// input, dialog requests, aborts, ...) that must all be delivered in order.
///
/// The numeric slot keeps keys from different variants disjoint (`Status` key
/// `"x"` and `Widget` id `"x"` are different slots), and the empty key turns
/// keyless variants into single global slots.
fn notification_state_slot(notification: &Notification) -> Option<(u8, &str)> {
    Some(match notification {
        Notification::Status { key, .. } => (0, key.as_str()),
        Notification::Widget { id, .. } => (1, id.as_str()),
        Notification::ToolUpdate { tool_call_id, .. } => (2, tool_call_id.as_str()),
        Notification::SetLabel { entry_id, .. } => (3, entry_id.as_str()),
        Notification::SetTitle(_) => (4, ""),
        Notification::WorkingMessage(_) => (5, ""),
        Notification::WorkingVisible(_) => (6, ""),
        Notification::WorkingIndicator(_) => (7, ""),
        Notification::HiddenThinkingLabel(_) => (8, ""),
        Notification::Footer(_) => (9, ""),
        Notification::Header(_) => (10, ""),
        Notification::SetSessionName(_) => (11, ""),
        Notification::SetTheme(_) => (12, ""),
        Notification::SetModel { .. } => (13, ""),
        Notification::SetThinkingLevel(_) => (14, ""),
        Notification::ActiveToolsOverride(_) => (15, ""),
        Notification::EditorSetText(_) => (16, ""),
        _ => return None,
    })
}
