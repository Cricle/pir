use super::*;

use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------
// Renderer-callback UI guard
// ---------------------------------------------------------------------------

/// One runtime's guard entry: `(active callbacks, has_ui to restore)`.
type RendererUiGuardEntry = (usize, bool);
/// Per-runtime renderer UI guards keyed by the shared `RuntimeState` `Arc`
/// address.
type RendererUiGuardMap = std::collections::HashMap<usize, RendererUiGuardEntry>;

/// Per-runtime renderer UI guards, keyed by the shared `RuntimeState` `Arc`
/// address (the same per-runtime identity the session-stream cells use).
fn renderer_ui_guards() -> std::sync::MutexGuard<'static, RendererUiGuardMap> {
    static GUARDS: std::sync::OnceLock<StdMutex<RendererUiGuardMap>> = std::sync::OnceLock::new();
    GUARDS
        .get_or_init(StdMutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// RAII switch that makes the runtime's async `ctx.ui.*` dialogs resolve
/// immediately for the duration of a renderer callback.
///
/// Renderer / widget / footer / header factory callbacks are invoked from
/// [`ExtensionHandle::invoke_renderer_sync`] on the TUI main thread, which
/// blocks until the actor replies. An async `ctx.ui.select/confirm/input/
/// editor` call inside such a callback parks a
/// [`crate::extensions::Notification::DialogRequest`] in the runtime state
/// whose only drainer is that blocked thread: the request can never be
/// answered, and the callback is aborted by the renderer deadline, leaving an
/// orphaned dialog queued for a later frame. The dialog host fn checks
/// `config.has_ui` on every call, so clearing it here makes the request
/// resolve `null`/`undefined` immediately (the headless-host contract)
/// instead of parking. Synchronous UI (`ctx.ui.notify` / `setStatus` /
/// `setTitle` / editor text) never consults `has_ui` and keeps working.
///
/// The switch is scoped to the callback's in-flight window and restored on
/// every exit path (early enqueue failure, reply, timeout); a nested renderer
/// call on the same runtime only restores the original value when the
/// outermost one leaves.
struct RendererUiGuard {
    key: usize,
    state: Arc<StdMutex<RuntimeState>>,
}

impl RendererUiGuard {
    fn enter(state: &Arc<StdMutex<RuntimeState>>) -> Self {
        let key = Arc::as_ptr(state) as usize;
        let mut guards = renderer_ui_guards();
        let entry = guards.entry(key).or_insert((0, false));
        if entry.0 == 0 {
            let mut runtime = crate::extensions::runtime::lock_state(state);
            entry.1 = runtime.config.has_ui;
            runtime.config.has_ui = false;
        }
        entry.0 += 1;
        drop(guards);
        Self {
            key,
            state: Arc::clone(state),
        }
    }
}

impl Drop for RendererUiGuard {
    fn drop(&mut self) {
        let mut guards = renderer_ui_guards();
        let Some(entry) = guards.get_mut(&self.key) else {
            return;
        };
        entry.0 = entry.0.saturating_sub(1);
        if entry.0 > 0 {
            return;
        }
        let restore = entry.1;
        guards.remove(&self.key);
        // Restore while still holding the map lock: a concurrent `enter` (which
        // takes the same map lock before reading `has_ui`) can then neither see
        // the suppressed value nor race this restore.
        crate::extensions::runtime::lock_state(&self.state)
            .config
            .has_ui = restore;
    }
}

impl ExtensionHandle {
    /// Start an incremental streaming JS hook call (the `__pir_stream:`
    /// `streamSimple` callback, upstream's live
    /// `AssistantMessageEventStream`) and return the receiver of its
    /// [`crate::extensions::runtime::HookStreamItem`]s. The stream always
    /// terminates with `End` (clean finish) or `Failed` (fail-closed error);
    /// dropping the receiver cancels the pull loop and closes the JS-side
    /// session. Runs under the long
    /// [`crate::extensions::runtime::HOOK_CALL_TIMEOUT`], per round trip.
    pub async fn call_hook_stream(
        &self,
        hook_key: &str,
        payload: &JsonValue,
    ) -> mpsc::Receiver<crate::extensions::runtime::HookStreamItem> {
        // Plan-only handle: no JS hook (reserved `__pir_stream:` /
        // `__pir_route:` callbacks are registered only by JS extensions) can
        // exist. Answer fail-closed like a dead actor so a consumer never
        // waits on an empty stream.
        let Some(tx) = self.tx.clone() else {
            let (dead_tx, dead_rx) = mpsc::channel(1);
            let _ = dead_tx
                .send(crate::extensions::runtime::HookStreamItem::Failed(
                    "extension runtime is not running (builtin-only plan)".to_owned(),
                ))
                .await;
            return dead_rx;
        };
        let (events_tx, events_rx) = mpsc::channel(64);
        self.command_queued();
        if tx
            .send(Command::CallHookStream {
                hook_key: hook_key.to_owned(),
                payload: payload.clone(),
                events: events_tx,
            })
            .await
            .is_err()
        {
            self.command_cancelled();
            // Dead actor: hand the consumer a fail-closed terminal instead of
            // a silently empty stream.
            let (dead_tx, dead_rx) = mpsc::channel(1);
            let _ = dead_tx
                .send(crate::extensions::runtime::HookStreamItem::Failed(
                    "extension actor is not running".to_owned(),
                ))
                .await;
            return dead_rx;
        }
        events_rx
    }

    /// Snapshot of registered commands after the last load.
    pub fn command_defs(&self) -> Vec<crate::extensions::CommandDef> {
        crate::extensions::runtime::lock_state(&self.state)
            .command_defs
            .clone()
    }

    /// Extensions that loaded successfully (last `load` call).
    pub fn loaded(&self) -> Vec<LoadedExtension> {
        self.loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Flags registered during load.
    pub fn flags(&self) -> Vec<crate::extensions::FlagDef> {
        crate::extensions::runtime::lock_state(&self.state)
            .flags
            .clone()
    }

    /// The sanitized identity (the entry file stem/name, never the full user
    /// path) of the extension a registry entry belongs to. Used by
    /// `--help-extensions` for the upstream "Registered by <extension>"
    /// fallback description; unknown ids yield `None`.
    pub fn extension_identity(&self, ext_id: u64) -> Option<String> {
        crate::extensions::runtime::lock_state(&self.state)
            .ext_identities
            .get(&ext_id)
            .cloned()
    }

    /// Current value of a registered flag (CLI value > default); `None` when
    /// the flag was never registered by any extension.
    pub fn get_flag(&self, name: &str) -> Option<crate::extensions::FlagValue> {
        crate::extensions::runtime::lock_state(&self.state).flag_value(name)
    }

    /// Apply a CLI-provided value to a registered flag.
    pub fn set_flag_value(
        &self,
        name: &str,
        value: crate::extensions::FlagValue,
    ) -> Result<(), String> {
        crate::extensions::runtime::lock_state(&self.state).set_flag_value(name, value)
    }

    /// Drain accumulated side-effect notifications.
    pub fn drain_notifications(&self) -> Vec<crate::extensions::Notification> {
        let mut state = crate::extensions::runtime::lock_state(&self.state);
        state.filter_abandoned_notifications();
        std::mem::take(&mut state.notifications)
            .into_iter()
            .map(|(_, notification)| notification)
            .collect()
    }

    /// Drain only the notifications matching `predicate`, keeping the rest
    /// queued. The session host (actionable class) and the TUI drain
    /// (presentation class) partition the queue with this so shared-handle
    /// drains never steal each other's notifications.
    pub fn take_notifications(
        &self,
        predicate: impl Fn(&Notification) -> bool,
    ) -> Vec<Notification> {
        let mut state = crate::extensions::runtime::lock_state(&self.state);
        state.filter_abandoned_notifications();
        let (taken, kept) = state
            .notifications
            .drain(..)
            .partition(|(_, notification)| predicate(notification));
        state.notifications = kept;
        taken
            .into_iter()
            .map(|(_, notification)| notification)
            .collect()
    }

    /// How many notifications are currently queued.
    pub fn pending_notification_count(&self) -> usize {
        crate::extensions::runtime::lock_state(&self.state)
            .notifications
            .len()
    }

    /// Queue one host-originated notification on the SAME bounded queue the
    /// JS host functions write (`ctx.ui.*`, `pi.sendMessage`, ...).
    ///
    /// This is the native [`crate::plugins::services::UiService`]'s write
    /// seam: a native plugin's `ctx.ui.notify` / `setWorkingMessage` /
    /// `setTitle` lands in the exact projection a JS extension produces, so
    /// the TUI drain, the headless native mirror, and `take_notifications`
    /// never need a second queue or a second payload shape. The queue's
    /// overflow policy (see `RuntimeState::push`) applies unchanged.
    pub fn push_notification(&self, notification: Notification) {
        crate::extensions::runtime::lock_state(&self.state).push_host(notification);
    }

    /// The theme registry pushed by the UI host
    /// (`{themes: [{name, path}], current, snapshots}`); empty JSON when the
    /// host never pushed one. Backs the native `UiService` theme reads.
    pub fn theme_registry(&self) -> JsonValue {
        crate::extensions::runtime::lock_state(&self.state)
            .theme_registry
            .clone()
    }

    /// Record `name` as the registry's current theme (the native twin of the
    /// `setThemeJson` host fn's bookkeeping; the TUI applies the actual
    /// palette when it drains the `SetTheme` notification).
    pub fn set_current_theme(&self, name: &str) {
        let mut state = crate::extensions::runtime::lock_state(&self.state);
        state.theme_registry["current"] = JsonValue::String(name.to_owned());
    }

    /// The live editor text snapshot pushed by the UI host
    /// (`ExtensionHandle::update_editor_state`); empty when no editor pushed
    /// one. Backs the native `UiService::editor_text` read.
    pub fn editor_text(&self) -> String {
        crate::extensions::runtime::lock_state(&self.state)
            .editor_text
            .clone()
    }

    /// Record the resolved extension path plan (CLI paths first, then
    /// settings/default paths; `builtin:<name>` entries included in order).
    /// The unified plugin host derives plugin dispatch order from it.
    pub fn set_path_plan(&self, plan: Vec<PathBuf>) {
        *self
            .path_plan
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = plan;
    }

    /// The recorded path plan (empty when none was recorded).
    pub fn path_plan(&self) -> Vec<PathBuf> {
        self.path_plan
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether the runtime has extension file paths to reload.
    ///
    /// `/reload` (upstream `session.reload`) rebuilds the JS runtime and
    /// re-executes the factory of every path the last `load` recorded, so a
    /// runtime with no load paths (a plan-only builtin-only handle, or one
    /// whose load never ran) has no JS half to reload — [`Self::reload`]
    /// answers "no extensions have been loaded yet" there. Hosts use this
    /// probe to skip that half and still reload the native plugin set.
    pub fn has_load_paths(&self) -> bool {
        !crate::extensions::runtime::lock_state(&self.state)
            .load_paths
            .is_empty()
    }

    /// A snapshot of the runtime configuration.
    pub fn config(&self) -> ExtensionConfig {
        crate::extensions::runtime::lock_state(&self.state)
            .config
            .clone()
    }

    /// The runtime's shared `ctx.ui.getToolsExpanded` / `setToolsExpanded`
    /// cell (upstream types.ts:296-299).
    ///
    /// This is the ONE cell both faces use: the JS host functions
    /// (`getToolsExpanded` / `setToolsExpanded`) read and write it, and the
    /// TUI clones it for tool-call rendering and its Ctrl+O toggle. No second
    /// copy exists, so an extension toggle restyles the live transcript and a
    /// TUI toggle is visible to the next extension read.
    pub fn tools_expanded_cell(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&crate::extensions::runtime::lock_state(&self.state).tools_expanded)
    }

    /// Current `ctx.ui.getToolsExpanded()` value (the shared cell's state).
    pub fn tools_expanded(&self) -> bool {
        crate::extensions::runtime::lock_state(&self.state)
            .tools_expanded
            .load(Ordering::SeqCst)
    }

    /// Set the shared tools-expanded cell (the native/TUI write side of
    /// `ctx.ui.setToolsExpanded`).
    pub fn set_tools_expanded(&self, expanded: bool) {
        crate::extensions::runtime::lock_state(&self.state)
            .tools_expanded
            .store(expanded, Ordering::SeqCst);
    }

    /// Push the host tool defs (the session's core/builtin tools plus the
    /// Rust-backend tools of the unified plugin registry) so the JS
    /// `getActiveTools`/`getAllTools` facades see non-extension tools with
    /// their real description and parameter schema.
    pub fn update_core_tools(&self, defs: Vec<crate::extensions::runtime::ToolDef>) {
        let mut state = crate::extensions::runtime::lock_state(&self.state);
        state.core_tools = defs;
        // The memoized `getAllToolsJson` array embeds these defs; its
        // fingerprint would already miss, but invalidating here keeps the
        // write site explicit.
        state.all_tools_json = None;
    }

    /// The persisted `setActiveTools` override, if any.
    pub fn active_tools_override(&self) -> Option<Vec<String>> {
        crate::extensions::runtime::lock_state(&self.state)
            .active_tools_override
            .clone()
    }

    // ------------------------------------------------------------------
    // Fast-path accessors (shared RuntimeState, no mailbox round-trip)
    // ------------------------------------------------------------------

    /// Whether any active extension registered a handler for `event`. Backed
    /// by the registry snapshot refreshed after every load/dispatch, so the
    /// agent loop can skip emission round-trips for unobserved events.
    ///
    /// The snapshot is refreshed at [`crate::extensions::runtime`]'s
    /// load/`emit`/`dispatch_command`/`invoke_shortcut` completion points
    /// (`runtime.rs`, `dispatch.rs`) and — counts-only
    /// (`ExtensionRuntime::refresh_handler_counts`) — at the tail of the other
    /// JS entry points the actor runs (`call_tool`, `invoke_renderer`,
    /// `argument_completions`, `extension_suggestions`); `/reload` clears it.
    ///
    /// A snapshot that is *possibly older* than the live JS table must never
    /// answer `false`: that would skip a live handler's event. So staleness is
    /// decided by a generation pair. JS bumps `RuntimeState::handlers_gen` on
    /// every handler-table mutation (`handlersDirty`, called at the `pi.on`
    /// register/unregister sites and the direct `refreshModels` row); every
    /// publisher stamps `handlers_snapshot_gen` with the generation it read
    /// *before* its export, next to the counts that export produced. Unequal
    /// generations mean a registration may have landed after the snapshot was
    /// taken — including the previously uncovered case of JS resumed with no
    /// command in flight (a `setTimeout`/promise continuation, a
    /// `__pir_stream:` hook callback) — so this answers `true`: the caller
    /// pays one emission round-trip that may find nothing (harmless), never a
    /// skipped handler. Equal generations mean no mutation since that export,
    /// so the counts are exact for this check.
    ///
    /// Both generations are plain `u64` fields of the same mutex-guarded
    /// [`crate::extensions::runtime::RuntimeState`] as the counts, read under
    /// this one lock acquisition: calling this synchronously from any thread
    /// is exactly as safe as the counts read it guards, with no new
    /// synchronization. `PluginHost::has_handlers` documents the same
    /// invariant from the host side.
    pub fn has_handlers(&self, event: &str) -> bool {
        let state = crate::extensions::runtime::lock_state(&self.state);
        // Conservative staleness short-circuit: a dirty generation can only
        // over-approximate (emit an event nobody handles), never miss one.
        if state.handlers_gen != state.handlers_snapshot_gen {
            return true;
        }
        state
            .handler_counts
            .get(event)
            .is_some_and(|count| *count > 0)
    }

    /// Whether the JS face has any live `ctx.ui.onTerminalInput` handler.
    ///
    /// One atomic load: the prelude publishes `state.terminalInputHandlers`
    /// through the `terminalInputHandlers` host function on every
    /// register/unregister, so the TUI's per-physical-input fast path never
    /// takes the runtime state lock and a session with no handlers pays
    /// nothing beyond that load.
    pub fn has_terminal_input_handlers(&self) -> bool {
        self.terminal_input_handlers
            .load(std::sync::atomic::Ordering::Acquire)
            > 0
    }

    /// Monotone generation of the runtime's JS definition writes — the O(1)
    /// value [`crate::plugins::host::PluginHost`] installs as its
    /// [`crate::plugins::host::JsDefGenerationProbe`].
    ///
    /// It moves on every write to a class the host's registry gate
    /// fingerprints (JS tools/commands/shortcuts via `registryDirty` — which
    /// also moves the export generation — and flags/providers/MCP
    /// servers/virtual models via their own host fns, plus the
    /// `/reload` registry clear), so an unchanged value proves no definition
    /// write happened since the host's committed snapshot and the host skips
    /// its deep fingerprint read. A moved value only makes the host re-read
    /// and compare; the comparison stays the rebuild decision, so a spurious
    /// move costs one read and never a spurious rebuild.
    ///
    /// Same shape as [`Self::has_handlers`]: a plain `u64` read under the
    /// mutex-guarded runtime state (one word, no mailbox round-trip), which is
    /// what makes the probe cheap enough to install on the per-emit path.
    pub fn defs_gen(&self) -> u64 {
        crate::extensions::runtime::lock_state(&self.state).defs_gen
    }

    /// Snapshot of registered shortcuts (TUI host enumeration).
    pub fn shortcuts(&self) -> Vec<ShortcutDef> {
        crate::extensions::runtime::lock_state(&self.state)
            .shortcut_defs
            .clone()
    }

    /// Extension-registered provider definitions (`pi.registerProvider`),
    /// each `{id, config}` JSON as provided. The session host converts them
    /// into real registry providers.
    pub fn provider_defs(&self) -> Vec<JsonValue> {
        crate::extensions::runtime::lock_state(&self.state)
            .provider_defs
            .clone()
    }

    /// Extension-registered MCP servers (`pi.registerMcpServer`), each
    /// `{name, config}` JSON as provided. The `mcp` builtin consumes them at
    /// the next registry rebuild.
    pub fn mcp_server_defs(&self) -> Vec<JsonValue> {
        crate::extensions::runtime::lock_state(&self.state)
            .mcp_server_defs
            .clone()
    }

    /// Push the live session/model snapshot backing the `ctx` facade
    /// (`ctx.model`, `ctx.sessionManager`, `ctx.getContextUsage`,
    /// `ctx.hasPendingMessages`, ...). The session host calls this before
    /// every prompt; the runtime only reads it.
    pub fn update_session_context(&self, snapshot: SessionContextSnapshot) {
        self.update_session_context_shared(Arc::new(snapshot));
    }

    /// [`Self::update_session_context`] for a snapshot the caller already
    /// shares through an `Arc`.
    ///
    /// The plugin host keeps the same `Arc` for its native readers; handing
    /// the runtime that reference is one refcount bump instead of deep-
    /// copying the snapshot (whose `entries` field is the serialized whole
    /// transcript) on every prompt push.
    pub fn update_session_context_shared(&self, snapshot: Arc<SessionContextSnapshot>) {
        let mut state = crate::extensions::runtime::lock_state(&self.state);
        state.session = snapshot;
    }

    /// Push the serialized model registry snapshot backing
    /// `ctx.modelRegistry` (available models + provider ids).
    pub fn update_model_registry(&self, models: JsonValue, providers: JsonValue) {
        let mut state = crate::extensions::runtime::lock_state(&self.state);
        state.model_registry = serde_json::json!({ "models": models, "providers": providers });
    }

    /// Push the serialized scoped-model list backing `ctx.scopedModels`.
    pub fn update_scoped_models(&self, scoped: JsonValue) {
        let mut state = crate::extensions::runtime::lock_state(&self.state);
        state.scoped_models = scoped;
    }

    /// Push the host completion capability backing
    /// `ctx.modelRegistry.complete` for providers the host executes
    /// (`__pir_host.modelCompleteJson`). The session host passes
    /// `AgentSession::complete_fn_for(current model)` together with the model
    /// identity it was built for; the stored closure runs against the
    /// session's provider registry with the session's request options
    /// (api key/thinking/retry/hooks). Passing `None` clears the capability
    /// (the JS side then fails closed with a clear error).
    pub fn set_complete_fn(
        &self,
        provider: &str,
        model_id: &str,
        complete: Arc<crate::compaction::CompleteFn>,
    ) {
        let mut state = crate::extensions::runtime::lock_state(&self.state);
        state.host_completion = Some(crate::extensions::runtime::HostCompletion {
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
            complete,
        });
    }

    /// Clear the host completion capability (`__pir_host.modelCompleteJson`
    /// then fails closed with a clear error).
    pub fn clear_complete_fn(&self) {
        let mut state = crate::extensions::runtime::lock_state(&self.state);
        state.host_completion = None;
    }

    /// Push the current thinking level (read by `ctx.thinkingLevel` /
    /// `pi.getThinkingLevel`).
    pub fn set_thinking_level_value(&self, level: &str) {
        crate::extensions::runtime::lock_state(&self.state).thinking_level = level.to_string();
    }

    /// Push the merged effective settings snapshot backing `pi.getSettings()`.
    pub fn update_settings(&self, settings: JsonValue) {
        crate::extensions::runtime::lock_state(&self.state).settings = settings;
    }

    /// Push the builtin (Rust plugin) commands so `getCommands()` reports
    /// them alongside extension commands.
    pub fn update_builtin_commands(&self, commands: Vec<crate::extensions::CommandDef>) {
        crate::extensions::runtime::lock_state(&self.state).builtin_commands = commands;
    }

    /// Push the provider ids that hold a usable credential, backing the
    /// `pi.setModel` `false` result for unauthenticated providers.
    pub fn update_authenticated_providers(&self, providers: Vec<String>) {
        crate::extensions::runtime::lock_state(&self.state).authenticated_providers = providers;
    }

    /// Bind the nested-call executor for an in-flight tool call id so the JS
    /// `ctx.executeTool` host fn can round-trip nested calls. The executor is
    /// already scoped to `call_id` by the scheduler.
    ///
    /// The executor is stored behind an `Arc` so every `ctx.executeTool`
    /// lookup is a refcount bump instead of re-cloning the executor's
    /// `caller_id` `String`; the parameter takes that same `Arc` (moved in),
    /// so binding allocates nothing — the caller shares the executor it
    /// already holds rather than boxing a fresh copy per call.
    pub fn bind_nested_executor(
        &self,
        call_id: &str,
        executor: Arc<crate::agent::nested_tool_calls::NestedToolExecutor>,
    ) {
        crate::extensions::runtime::lock_state(&self.state)
            .nested_executors
            .insert(call_id.to_owned(), executor);
    }

    /// Release the nested-call executor bound to `call_id`.
    pub fn unbind_nested_executor(&self, call_id: &str) {
        crate::extensions::runtime::lock_state(&self.state)
            .nested_executors
            .remove(call_id);
    }

    /// Deliver a dialog reply for `ctx.ui.select/confirm/input/editor`
    /// (TUI side of the request/reply channel). Wakes the awaiting host fn.
    ///
    /// P1-3: the wake goes through the PER-DIALOG Notify (one waiter per
    /// dialog id), so a reply can never wake the wrong waiter — with the
    /// old shared `notify_one`, a wrong-waiter wakeup consumed the permit
    /// and the answered dialog's JS await stalled for up to the dialog
    /// timeout.
    pub fn respond_dialog(&self, id: &str, value: JsonValue) {
        let mut state = crate::extensions::runtime::lock_state(&self.state);
        // Capture the live waiter's registration BEFORE the sweep: the reply
        // inherits the waiter's own effective ttl (request-level `timeout`
        // when it had one), so a short global dialog TTL can neither evict
        // the waiter nor the reply it is about to consume.
        let waiter = state.dialog_notifies.remove(id);
        // P2-9: sweep replies/waiters past their own ttl before recording
        // this one, so entries for dead waiters cannot accumulate.
        state.sweep_expired_dialog_state();
        let ttl = waiter
            .as_ref()
            .map(|(_, _, ttl)| *ttl)
            .unwrap_or(state.dialog_ttl);
        state
            .dialog_responses
            .insert(id.to_owned(), (value, std::time::Instant::now(), ttl));
        if let Some((notify, _, _)) = waiter {
            // `notify_one` stores a permit when the waiter has not polled
            // yet, so the wait is race-free against this insert-then-notify.
            notify.notify_one();
        }
    }

    /// How many dialog replies are currently stored (test probe for the
    /// P2-9 expiry sweep).
    pub fn dialog_response_count(&self) -> usize {
        crate::extensions::runtime::lock_state(&self.state)
            .dialog_responses
            .len()
    }

    /// Override the dialog reply/waiter TTL (default: the dialog timeout of
    /// 600s). Used by tests; the TTL drives the P2-9 expiry sweep and the
    /// dialog host fn's wait deadline.
    pub fn set_dialog_ttl(&self, ttl: std::time::Duration) {
        crate::extensions::runtime::lock_state(&self.state).dialog_ttl = ttl;
    }

    /// Renderer registrations exported from the JS registry (last refresh).
    pub fn renderer_defs(&self) -> JsonValue {
        crate::extensions::runtime::lock_state(&self.state)
            .renderer_defs
            .clone()
    }

    /// Push the current effective system prompt (read by
    /// `ctx.getSystemPrompt()`).
    pub fn set_system_prompt_value(&self, prompt: &str) {
        crate::extensions::runtime::lock_state(&self.state)
            .config
            .system_prompt = prompt.to_string();
    }

    /// Push the normalized `BuildSystemPromptOptions` snapshot backing
    /// `ctx.getSystemPromptOptions()` and the `before_agent_start` event
    /// (upstream `_baseSystemPromptOptions`, agent-session.ts:1735):
    /// `{cwd, customPrompt?, selectedTools, hiddenTools, toolSnippets,
    /// toolGuidelines, promptGuidelines, appendSystemPrompt, sections,
    /// contextFiles, skills}`.
    pub fn update_system_prompt_options(&self, options: JsonValue, key: String) {
        let mut state = crate::extensions::runtime::lock_state(&self.state);
        state.system_prompt_options = options;
        state.system_prompt_options_key = key;
    }

    /// The pushed options snapshot plus its fingerprint (see
    /// [`Self::update_system_prompt_options`]); `None` before the first push.
    pub fn system_prompt_options(&self) -> Option<(String, JsonValue)> {
        let state = crate::extensions::runtime::lock_state(&self.state);
        state.system_prompt_options.is_object().then(|| {
            (
                state.system_prompt_options_key.clone(),
                state.system_prompt_options.clone(),
            )
        })
    }

    /// Push the live editor text snapshot backing the sync
    /// `ctx.ui.getEditorText()` (the TUI refreshes it each frame drain;
    /// headless hosts never push, so the JS side always reads `""`).
    pub fn update_editor_state(&self, text: String) {
        crate::extensions::runtime::lock_state(&self.state).editor_text = text;
    }

    /// Push the theme registry backing `ui.getThemes()` / `ui.getTheme` /
    /// `ui.setTheme`:
    /// `{themes: [{name, path}], current: name, snapshots: {name: snapshot}}`.
    pub fn update_theme_registry(&self, registry: JsonValue) {
        crate::extensions::runtime::lock_state(&self.state).theme_registry = registry;
    }

    /// Whether any `ui.addAutocompleteProvider` suggester is registered
    /// (fast-path read before paying the async completion round-trip).
    pub fn has_autocomplete_suggesters(&self) -> bool {
        crate::extensions::runtime::lock_state(&self.state).has_autocomplete_suggesters
    }

    /// Invoke one extension renderer callback through the mailbox, bounded:
    /// the actor applies `timeout` as the QuickJS interrupt deadline (a
    /// runaway JS job is actually aborted) and this side additionally bounds
    /// its wait with `timeout` plus one second of slack so a busy actor
    /// degrades the rendering instead of wedging the UI loop.
    ///
    /// Safe from any thread, including a tokio runtime thread: the enqueue is
    /// a non-blocking `try_send` (tokio's `blocking_send` panics when called
    /// from within a runtime — which is exactly the TUI render path) backed
    /// by a bounded yield-spin for transient mailbox pressure. The actor's
    /// receive loop drains the mailbox independently of renderer execution,
    /// so a full queue means a stalled actor; the spin waits out scheduling
    /// transients, then degrades to an `Err` the call sites already render as
    /// a failure notice.
    pub fn invoke_renderer_sync(
        &self,
        request: &JsonValue,
        timeout: Duration,
    ) -> anyhow::Result<RendererOutcome> {
        self.invoke_renderer_sync_full(request, timeout)
            .map(|result| result.outcome)
    }

    /// [`Self::invoke_renderer_sync`] with the full envelope: the lines/text
    /// outcome plus the tool `renderShell` choice the callback reported
    /// (upstream `ToolDefinition.renderShell`, extensions/types.ts:595).
    ///
    /// Non-tool renderer calls answer `shell: None`; the field is only
    /// populated by the `toolDefinition` / `toolRenderers` bridge replies.
    pub fn invoke_renderer_sync_full(
        &self,
        request: &JsonValue,
        timeout: Duration,
    ) -> anyhow::Result<RendererCallOutcome> {
        // Plan-only handle: no renderer callback can be registered, which is
        // the empty runtime's "not handled" answer (the caller keeps its
        // default rendering). No mailbox, no UI guard.
        let Some(tx) = self.tx.clone() else {
            return Ok(RendererCallOutcome::default());
        };
        // The actor runs the callback while this thread blocks here; suppress
        // the runtime's async UI for that window so a `ctx.ui.*` dialog
        // resolves immediately instead of parking a request only this blocked
        // thread could drain (see `RendererUiGuard`).
        let _ui_guard = RendererUiGuard::enter(&self.state);
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let mut command = Command::InvokeRenderer {
            request: request.clone(),
            timeout,
            reply: reply_tx,
        };
        // Tradeoff: the mailbox stays bounded (capacity 64), so a stalled
        // actor can never make render attempts queue without limit; the rare
        // full-queue case burns up to ~50ms of well-intentioned yield-spin on
        // this thread before failing open to the default rendering, which
        // keeps the UI loop's worst-case latency bounded.
        let enqueue_deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        self.command_queued();
        loop {
            match tx.try_send(command) {
                Ok(()) => break,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.command_cancelled();
                    return Err(anyhow::anyhow!("extension actor is not running"));
                }
                Err(mpsc::error::TrySendError::Full(returned)) => {
                    if std::time::Instant::now() >= enqueue_deadline {
                        self.command_cancelled();
                        return Err(anyhow::anyhow!("extension actor mailbox is full"));
                    }
                    command = returned;
                    // Sleep, don't spin: a full mailbox means the actor needs
                    // this CPU to drain it. A 1 ms step keeps the worst-case
                    // enqueue latency inside the 50 ms budget while never
                    // burning a whole core alongside the actor (the constrained
                    // single-core targets would otherwise run the UI thread
                    // and the actor against each other).
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
        let slack = std::time::Duration::from_secs(1);
        let kind = request
            .get("kind")
            .and_then(JsonValue::as_str)
            .unwrap_or("?");
        let key = request.get("key").and_then(JsonValue::as_str).unwrap_or("");
        let started = std::time::Instant::now();
        let outcome = reply_rx.recv_timeout(timeout + slack);
        let elapsed = started.elapsed();
        if elapsed >= std::time::Duration::from_millis(250) {
            crate::ext_diag!(
                "[ext] renderer slow: {}ms kind={} key={} timeout={:?}",
                elapsed.as_millis(),
                kind,
                key,
                timeout
            );
        }
        match outcome {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Err(anyhow::anyhow!("renderer call timed out"))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!(
                "extension actor dropped the renderer reply"
            )),
        }
    }

    /// Invoke one registered tool's own renderer (upstream
    /// `ToolDefinition.renderCall` / `renderResult`, extensions/types.ts:
    /// 659-667): `render_kind` is `"call"` or `"result"`, `request` the
    /// `ToolRenderContext` wire (`{args, toolCallId, cwd, result?, ...}`).
    /// Returns the same [`RendererOutcome`] envelope as
    /// [`Self::invoke_renderer_sync`]; `lines: None` means the tool declares
    /// no renderer for that kind (the TUI keeps its default drawing).
    pub fn call_tool_renderer_sync(
        &self,
        tool_name: &str,
        render_kind: &str,
        request: &JsonValue,
        timeout: Duration,
    ) -> anyhow::Result<RendererOutcome> {
        self.call_tool_renderer_sync_full(tool_name, render_kind, request, timeout)
            .map(|result| result.outcome)
    }

    /// [`Self::call_tool_renderer_sync`] with the tool's `renderShell` choice
    /// (upstream `ToolDefinition.renderShell`, extensions/types.ts:595). The
    /// TUI's default-shell painting reads it; legacy `None` keeps the bare
    /// renderer lines.
    pub fn call_tool_renderer_sync_full(
        &self,
        tool_name: &str,
        render_kind: &str,
        request: &JsonValue,
        timeout: Duration,
    ) -> anyhow::Result<RendererCallOutcome> {
        self.invoke_renderer_sync_full(
            &serde_json::json!({
                "kind": "toolDefinition",
                "key": tool_name,
                "renderKind": render_kind,
                "payload": request,
            }),
            timeout,
        )
    }

    /// Run `f` inside the same renderer UI guard
    /// [`Self::invoke_renderer_sync`] holds for JS callbacks.
    ///
    /// Native plugin renderers (the `RendererService` registry) execute as
    /// Rust vtables on the TUI thread and never cross the QuickJS mailbox, so
    /// they do not pass through `invoke_renderer_sync`. They still run under
    /// the same re-entrancy rule (an async `ctx.ui.*` dialog requested while a
    /// render callback holds the UI thread would park forever), so the TUI
    /// wraps every native render call in this helper. The guard is scoped,
    /// nestable, and restores `has_ui` on every exit path.
    pub fn with_renderer_ui_guard<T>(&self, f: impl FnOnce() -> T) -> T {
        let _ui_guard = RendererUiGuard::enter(&self.state);
        f()
    }

    /// Whether a renderer / widget / footer / header factory callback
    /// currently holds the async-UI suppression guard for this runtime (see
    /// [`RendererUiGuard`]). Diagnostics/test seam.
    pub fn renderer_callback_in_flight(&self) -> bool {
        let key = Arc::as_ptr(&self.state) as usize;
        renderer_ui_guards().contains_key(&key)
    }

    /// Fetch argument completions for a slash command (async; the TUI calls
    /// this from its extension dispatcher thread).
    pub async fn argument_completions(
        &self,
        command: &str,
        prefix: &str,
    ) -> anyhow::Result<Vec<JsonValue>> {
        // Plan-only handle: no JS command exists, so the empty runtime's
        // "no completions" answer.
        let Some(tx) = self.tx.clone() else {
            return Ok(Vec::new());
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_queued();
        if let Err(error) = tx
            .send(Command::GetArgCompletions {
                command: command.to_owned(),
                prefix: prefix.to_owned(),
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
            .map_err(|_| anyhow::anyhow!("extension actor dropped the completions reply"))?
    }

    /// Consult the `ui.addAutocompleteProvider` suggesters (async; the TUI
    /// calls this from its extension dispatcher thread).
    pub async fn extension_suggestions(
        &self,
        context_json: &str,
    ) -> anyhow::Result<Vec<JsonValue>> {
        // Plan-only handle: no suggesters can be registered.
        let Some(tx) = self.tx.clone() else {
            return Ok(Vec::new());
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_queued();
        if let Err(error) = tx
            .send(Command::GetSuggestions {
                context_json: context_json.to_owned(),
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
            .map_err(|_| anyhow::anyhow!("extension actor dropped the suggestions reply"))?
    }

    /// Override the per-call JS dispatch timeout (default
    /// [`crate::extensions::EXTENSION_CALL_TIMEOUT`]). Used by tests; the
    /// timeout is enforced as a QuickJS interrupt-handler deadline, so a
    /// runaway JS job is actually aborted and the runtime stays usable.
    pub fn set_call_timeout(&self, timeout: std::time::Duration) {
        crate::extensions::runtime::lock_state(&self.state).call_timeout = timeout;
    }

    /// The effective per-call JS dispatch timeout (the resolved
    /// [`crate::extensions::limits::ExtensionLimits::call_timeout`], or a
    /// later [`Self::set_call_timeout`] override). Host faces that must give a
    /// JS handler a fair budget (raw terminal input) read it instead of
    /// hard-coding a short timeout.
    pub fn call_timeout(&self) -> std::time::Duration {
        crate::extensions::runtime::lock_state(&self.state).call_timeout
    }

    /// Bind one streamSimple request's options hooks (upstream
    /// `SimpleStreamOptions.onPayload` / `onResponse` /
    /// `onProviderStreamEvent`) under `session`; the JS `options.onPayload`/
    /// `onResponse`/`onProviderStreamEvent` wrappers fire them through
    /// `__pir_host.streamHookEvent`. Fast-path shared-state write (no mailbox
    /// round-trip); callers unbind with [`Self::unbind_stream_hooks`] when the
    /// stream ends.
    pub fn bind_stream_hooks(
        &self,
        session: &str,
        hooks: crate::extensions::runtime::StreamHookBridge,
    ) {
        crate::extensions::runtime::lock_state(&self.state)
            .stream_hook_bridges
            .insert(session.to_owned(), hooks);
    }

    /// Release the options-hook bridge bound to `session`.
    pub fn unbind_stream_hooks(&self, session: &str) {
        crate::extensions::runtime::lock_state(&self.state)
            .stream_hook_bridges
            .remove(session);
    }
}
