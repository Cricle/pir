use super::*;

use std::sync::Weak;
use std::time::Duration;

use crate::agent::agent_loop::BoxFuture;
use crate::extensions::runtime::EventBacklog;
use crate::plugins::plugin::services::ToolCallRef;

// ---------------------------------------------------------------------------
// Native session-level event fan-out
// ---------------------------------------------------------------------------

/// Bound on queued native session-event dispatches. Session-level events are
/// low-frequency (a queue change, one compaction lifecycle, a retry), so a
/// small queue is plenty; a pathological producer drops the newest event
/// instead of growing memory, the same policy as the tool-update channel.
const NATIVE_EVENT_BACKLOG_BOUND: usize = 64;

/// Wall-clock cap on one fire-and-forget native/session event dispatch (the
/// same bound the live tool-update channel applies). A slow Rust plugin never
/// delays the session: the dispatch is abandoned and the event is dropped.
const NATIVE_EVENT_DISPATCH_TIMEOUT: Duration = Duration::from_secs(10);

/// One queued native session-level event dispatch.
struct NativeSessionEvent {
    plugins: Arc<PluginHost>,
    event: String,
    payload: JsonValue,
    /// `true` for events that originated as a [`SessionEvent`]: those are the
    /// session's own events and go through [`PluginHost::emit`], which reaches
    /// the JS adapter and the native plugins exactly once (the JS face has no
    /// other delivery path for them). `false` for notification-derived events
    /// (`status` / `extension_ui_request`): the JS runtime PRODUCED those, so
    /// only the native face observes them.
    both_faces: bool,
}

fn dispatch_native_session_event(item: NativeSessionEvent) -> BoxFuture<'static, ()> {
    Box::pin(async move {
        let interested = if item.both_faces {
            item.plugins.has_handlers(&item.event)
        } else {
            item.plugins.has_native_handlers(&item.event)
        };
        // Nobody declared interest: skip the payload move and the dispatch.
        if !interested {
            return;
        }
        let NativeSessionEvent {
            plugins,
            event,
            mut payload,
            both_faces,
        } = item;
        let dispatch = async {
            if both_faces {
                if let Err(error) = plugins.emit(&event, &mut payload).await {
                    crate::ext_diag!("[ext] {event} dispatch failed: {error}");
                }
            } else {
                plugins.emit_native(&event, &mut payload).await;
            }
        };
        if tokio::time::timeout(NATIVE_EVENT_DISPATCH_TIMEOUT, dispatch)
            .await
            .is_err()
        {
            crate::ext_diag!("[ext] {event} native dispatch timed out");
        }
    })
}

type SessionEventHandlers = std::sync::RwLock<Vec<SessionEventHandler>>;

/// One session's ordered native-event channel: registered as a
/// [`SessionEventHandler`] (so `SessionEvent`s flow into it) and reachable from
/// the notification projection (so `status` / `extension_ui_request` share the
/// same queue and ordering).
struct NativeEventBridge {
    /// The session's handler list. Weak: the bridge dies with the session
    /// (its only strong owner is the subscriber handler stored in that list).
    events: Weak<SessionEventHandlers>,
    /// One worker per session: the bounded FIFO that keeps session-event
    /// ordering while never blocking the emitter. It is spawn-on-first-push
    /// in whatever runtime the session runs on.
    backlog: EventBacklog<NativeSessionEvent>,
}

/// Installed bridges, keyed by the allocation identity of each session's
/// `session_events` list. Weak: the subscriber handler owns the bridge, so a
/// dropped session releases its queue and worker; dead entries are pruned on
/// the next lookup.
static NATIVE_SESSION_EVENT_BRIDGES: Mutex<Vec<Weak<NativeEventBridge>>> = Mutex::new(Vec::new());

/// The bridge for `events`, installing it (and the `SessionEvent` subscriber)
/// when absent. `None` when the host has no native (non-JS) plugins — there is
/// nothing to deliver to.
fn native_event_bridge(
    events: &Arc<SessionEventHandlers>,
    plugins: &Arc<PluginHost>,
) -> Option<Arc<NativeEventBridge>> {
    if !plugins.has_non_js_plugins() {
        return None;
    }
    let mut bridges = NATIVE_SESSION_EVENT_BRIDGES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    bridges.retain(|bridge| bridge.strong_count() > 0);
    for bridge in bridges.iter() {
        if let Some(existing) = bridge.upgrade() {
            if std::ptr::eq(existing.events.as_ptr(), Arc::as_ptr(events)) {
                return Some(existing);
            }
        }
    }
    let bridge = Arc::new(NativeEventBridge {
        events: Arc::downgrade(events),
        backlog: EventBacklog::new(
            NATIVE_EVENT_BACKLOG_BOUND,
            Arc::new(dispatch_native_session_event),
        ),
    });
    // The handler owns the bridge (the registry entry is only a Weak): it is
    // dropped together with the session's subscriber list, which releases the
    // queue and its worker.
    let handler_bridge = Arc::clone(&bridge);
    let handler_plugins = Arc::clone(plugins);
    let handler: SessionEventHandler = Arc::new(move |event: SessionEvent| {
        let payload = session_event_to_json(&event);
        let Some(name) = payload
            .get("type")
            .and_then(JsonValue::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        // `session_info_changed` is also an upstream EXTENSION event
        // (`_extensionRunner.emit(event)` beside `_emit(event)`). The name
        // mutator emits that extension face directly, so forwarding the
        // session event here would hand both faces a duplicate.
        if name == "session_info_changed" {
            return;
        }
        // Synchronous and non-blocking by contract; outside a tokio runtime
        // the event is dropped (counted by the backlog), never a panic.
        handler_bridge.backlog.push_with(|| NativeSessionEvent {
            plugins: Arc::clone(&handler_plugins),
            event: name,
            payload,
            both_faces: true,
        });
    });
    // The registry lock is held across the push so two concurrent installs
    // for the same session cannot both register a subscriber. The handler
    // list never takes this lock, so the ordering cannot deadlock.
    events
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(handler);
    bridges.push(Arc::downgrade(&bridge));
    Some(bridge)
}

/// Register the native fan-out on a session's [`SessionEvent`] stream exactly
/// once.
///
/// Every session-level event outside the agent-loop enum (upstream
/// `AgentSessionEvent`: `queue_update`, `compaction_start`/`_end`, the
/// `auto_retry_*` lifecycle, `bash_execution_update`, `background_tool_end`)
/// is emitted to the `session_events` subscribers. This installs one
/// subscriber that serializes each event with the SAME
/// [`session_event_to_json`] payload the RPC/TUI hosts see and hands it to
/// [`PluginHost::emit`] on a bounded, ordered backlog — so both faces observe
/// those events exactly once and a slow handler never blocks the emitting
/// session. The call is idempotent per session; the first run (or an idle
/// notification apply) installs it before any of those events can fire.
pub(super) fn install_native_session_event_bridge(
    events: &Arc<SessionEventHandlers>,
    plugins: &Arc<PluginHost>,
) {
    let _ = native_event_bridge(events, plugins);
}

/// Push one notification-derived event (`status` / `extension_ui_request`)
/// onto the session's ordered native queue.
fn push_native_notification_event(
    events: &Arc<SessionEventHandlers>,
    plugins: &Arc<PluginHost>,
    event: &'static str,
    payload: JsonValue,
) {
    let Some(bridge) = native_event_bridge(events, plugins) else {
        return;
    };
    bridge.backlog.push_with(|| NativeSessionEvent {
        plugins: Arc::clone(plugins),
        event: event.to_owned(),
        payload,
        both_faces: false,
    });
}

// ---------------------------------------------------------------------------
// Notification-shaped JS surfaces (`ctx.ui.*`)
// ---------------------------------------------------------------------------

/// Fire-and-forget ids for notification-derived requests without a dialog id,
/// matching the RPC protocol's `ui-<n>` shape.
static NOTIFICATION_EVENT_SEQ: AtomicU64 = AtomicU64::new(0);

fn fire_and_forget_event_id() -> String {
    format!(
        "ui-{}",
        NOTIFICATION_EVENT_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Map one extension notification to the event a native plugin observes.
///
/// These are notification-shaped in the JS path (the runtime queues a
/// [`Notification`] the UI host drains), so a native plugin would never see
/// them. The projection here is deliberately 1:1 with the documented RPC
/// wire shapes (`extension_ui_request` with `method`/`id`, and the status-line
/// event) so both faces read the same payload. Observing is all this lane
/// does: ANSWERING a UI request remains the future `UiService`.
fn native_notification_event(notification: &Notification) -> Option<(&'static str, JsonValue)> {
    match notification {
        Notification::Status { key, text } => Some((
            "status",
            serde_json::json!({
                "type": "status",
                "key": key,
                "text": text,
            }),
        )),
        Notification::Notify { message, level } => Some((
            "extension_ui_request",
            serde_json::json!({
                "type": "extension_ui_request",
                "id": fire_and_forget_event_id(),
                "method": "notify",
                "message": message,
                "notifyType": level,
            }),
        )),
        Notification::DialogRequest {
            id,
            kind,
            title,
            options,
        } => {
            let title = title
                .clone()
                .or_else(|| {
                    options
                        .get("title")
                        .and_then(JsonValue::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            let request = match kind.as_str() {
                "select" => serde_json::json!({
                    "type": "extension_ui_request",
                    "id": id.to_string(),
                    "method": "select",
                    "title": title,
                    "options": options.get("options").cloned().unwrap_or_else(|| serde_json::json!([])),
                }),
                "confirm" => serde_json::json!({
                    "type": "extension_ui_request",
                    "id": id.to_string(),
                    "method": "confirm",
                    "title": title,
                    "message": options
                        .get("message")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default(),
                }),
                "input" => {
                    let mut request = serde_json::json!({
                        "type": "extension_ui_request",
                        "id": id.to_string(),
                        "method": "input",
                        "title": title,
                    });
                    if let Some(placeholder) =
                        options.get("placeholder").and_then(JsonValue::as_str)
                    {
                        request["placeholder"] = serde_json::json!(placeholder);
                    }
                    if let Some(timeout) = options.get("timeout") {
                        request["timeout"] = timeout.clone();
                    }
                    request
                }
                "editor" => {
                    let mut request = serde_json::json!({
                        "type": "extension_ui_request",
                        "id": id.to_string(),
                        "method": "editor",
                        "title": title,
                    });
                    if let Some(prefill) = options.get("prefill").and_then(JsonValue::as_str) {
                        request["prefill"] = serde_json::json!(prefill);
                    }
                    request
                }
                _ => return None,
            };
            Some(("extension_ui_request", request))
        }
        // Presentation notifications with TUI-only semantics (widgets, title,
        // working message, editor text, ...) have no documented event shape;
        // they are left to the UI hosts.
        _ => None,
    }
}

impl AgentSession {
    /// Mirror the presentation-class extension notifications a HEADLESS
    /// session would otherwise never deliver to native plugins.
    ///
    /// `status` / `extension_ui_request` are notification-shaped in the JS
    /// path: the runtime queues a [`Notification`] and the UI host drains it.
    /// A headless session (no UI) has no such consumer, so the event is
    /// projected once for the native fan-out. UI hosts (TUI / RPC, both
    /// `has_ui`) keep ownership of the queue: taking status lines or dialog
    /// requests there would steal them. Dialog requests are only produced
    /// when `has_ui` is set, so a headless drain can never strand one.
    fn mirror_presentation_notifications_to_native(&self, plugins: &PluginHost) {
        if !plugins.has_non_js_plugins() {
            return;
        }
        let Some(js) = plugins.js_handle() else {
            return;
        };
        if js.config().has_ui {
            return;
        }
        let Some(host) = self.plugins.as_ref() else {
            return;
        };
        let notifications = plugins.take_notifications(|notification| {
            matches!(
                notification,
                Notification::Status { .. }
                    | Notification::Notify { .. }
                    | Notification::DialogRequest { .. }
            )
        });
        for notification in notifications {
            if let Some((event, payload)) = native_notification_event(&notification) {
                push_native_notification_event(&self.session_events, host, event, payload);
            }
            // Preserve the diagnostic the former overflow-only headless drain
            // printed for these notifications.
            print_extension_notification(&notification);
        }
    }

    /// Test seam: deliver one synthetic session-level event through the
    /// `session_events` subscribers (the exact path the queue/compaction/
    /// retry emission sites use), so a test can exercise the native fan-out
    /// without a real threshold crossing or a retryable provider. Installs
    /// the bridge first when the session has not run yet.
    #[doc(hidden)]
    pub fn emit_session_event_for_test(&self, event: SessionEvent) {
        if let Some(plugins) = &self.plugins {
            install_native_session_event_bridge(&self.session_events, plugins);
        }
        self.emit_session_event(event);
    }

    /// Test seam: project one extension notification through the native
    /// observation path (the headless `ctx.ui.*` mapping above) without a JS
    /// runtime. Used to pin `status` / `extension_ui_request` payloads.
    #[doc(hidden)]
    pub fn dispatch_extension_notification_for_test(&self, notification: &Notification) {
        let Some(plugins) = self.plugins.as_ref() else {
            return;
        };
        if let Some((event, payload)) = native_notification_event(notification) {
            push_native_notification_event(&self.session_events, plugins, event, payload);
        }
    }
}

// ---------------------------------------------------------------------------
// Host-level shutdown requests
// ---------------------------------------------------------------------------

/// One session's pending `ctx.shutdown()` request, keyed by the allocation
/// identity of its `session_events` list.
///
/// The TUI host takes the `Notification::ShutdownRequest` straight off the
/// shared extension queue (it owns the main loop and must react on the next
/// tick); headless hosts (RPC and any other `AgentSession`-backed host) let
/// the session consumer record it here and consume it at their own boundary
/// (`take_shutdown_request`). The value is a `Weak`, so an entry dies with its
/// session and a later session reusing the address starts clean — the same
/// liveness pattern as [`NATIVE_SESSION_EVENT_BRIDGES`].
struct ShutdownRequestLatch {
    session_events: Weak<SessionEventHandlers>,
}

static SHUTDOWN_REQUEST_LATCHES: Mutex<Vec<ShutdownRequestLatch>> = Mutex::new(Vec::new());

fn shutdown_request_latches() -> std::sync::MutexGuard<'static, Vec<ShutdownRequestLatch>> {
    SHUTDOWN_REQUEST_LATCHES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Drop entries whose session is gone (their `session_events` `Arc` died).
fn prune_shutdown_latches(latches: &mut Vec<ShutdownRequestLatch>) {
    latches.retain(|latch| latch.session_events.strong_count() > 0);
}

/// Record a shutdown request for `events`' session (idempotent).
fn mark_shutdown_request(events: &Arc<SessionEventHandlers>) {
    let mut latches = shutdown_request_latches();
    prune_shutdown_latches(&mut latches);
    let key = Arc::as_ptr(events);
    if latches
        .iter()
        .any(|latch| std::ptr::eq(latch.session_events.as_ptr(), key))
    {
        return;
    }
    latches.push(ShutdownRequestLatch {
        session_events: Arc::downgrade(events),
    });
}

/// Whether `events`' session has an unconsumed shutdown request.
fn shutdown_request_pending(events: &Arc<SessionEventHandlers>) -> bool {
    let mut latches = shutdown_request_latches();
    prune_shutdown_latches(&mut latches);
    let key = Arc::as_ptr(events);
    latches
        .iter()
        .any(|latch| std::ptr::eq(latch.session_events.as_ptr(), key))
}

/// Consume `events`' session shutdown request (one-shot).
fn take_shutdown_request(events: &Arc<SessionEventHandlers>) -> bool {
    let mut latches = shutdown_request_latches();
    prune_shutdown_latches(&mut latches);
    let key = Arc::as_ptr(events);
    if let Some(index) = latches
        .iter()
        .position(|latch| std::ptr::eq(latch.session_events.as_ptr(), key))
    {
        latches.swap_remove(index);
        true
    } else {
        false
    }
}

impl AgentSession {
    /// Record a host-level shutdown request (`ctx.shutdown`) for this
    /// session. The request is consumed by the host's exit path
    /// ([`AgentSession::take_shutdown_request`]) and never aborts a run.
    pub(super) fn mark_shutdown_requested(&self) {
        mark_shutdown_request(&self.session_events);
    }

    /// Whether `ctx.shutdown()` asked the host to exit and no host has
    /// consumed the request yet.
    pub fn shutdown_requested(&self) -> bool {
        shutdown_request_pending(&self.session_events)
    }

    /// Take (and clear) the host-level shutdown request, if any. Hosts that
    /// own an input loop (RPC) call this after each command and at
    /// `agent_settled`; `true` means "exit through the normal teardown".
    pub fn take_shutdown_request(&self) -> bool {
        take_shutdown_request(&self.session_events)
    }
}

impl AgentSession {
    /// `session_compact_failed` emission (manual / overflow paths; the
    /// threshold path emits inline from the compaction hook). Upstream
    /// `SessionCompactFailedEvent`: `errorMessage` is the non-abort failure
    /// text (`Compaction failed: …` / `Context overflow recovery failed: …`)
    /// and undefined when the compaction was aborted; `fromExtension` is true
    /// when the failing compaction content came from a `session_before_compact`
    /// handler.
    pub(super) async fn emit_compact_failed(
        &self,
        reason: &str,
        error_message: Option<String>,
        aborted: bool,
        from_extension: bool,
    ) {
        let Some(plugins) = &self.plugins else {
            return;
        };
        emit_plugin_event(
            plugins,
            "session_compact_failed",
            serde_json::json!({
                "type": "session_compact_failed",
                "reason": reason,
                "errorMessage": error_message,
                "aborted": aborted,
                "willRetry": false,
                "fromExtension": from_extension,
            }),
        )
        .await;
    }

    /// Deliver the one-time `session_start` lifecycle (and the notifications
    /// its handlers queue) EARLY, before a host entry point dispatches the
    /// extension `input` event on its own.
    ///
    /// [`AgentSession::start_run`] normally performs this block right before
    /// the `input` dispatch. The source-aware prompt entry points
    /// ([`AgentSession::prompt_with_source`]) must dispatch `input` before the
    /// run claims the session (so a `handled` outcome can swallow the prompt
    /// without starting a turn), which would otherwise let the first `input`
    /// handler of a fresh session observe a pre-`session_start` context. This
    /// runs the exact same once-guarded block, so `start_run` observes the
    /// flag and skips it — never a second `session_start`.
    pub(super) async fn ensure_session_start(&self) {
        let Some(plugins) = &self.plugins else {
            return;
        };
        if self.session_start_sent.swap(true, Ordering::SeqCst) {
            return;
        }
        if plugins.has_handlers("session_start") {
            let mut payload = serde_json::json!({
                "type": "session_start",
                "reason": "startup",
            });
            if let Err(error) = plugins.emit("session_start", &mut payload).await {
                crate::ext_diag!("[ext] session_start dispatch failed: {error}");
            }
        }
        // Handlers may have registered providers in session_start; actionable
        // notifications (setModel, setThinkingLevel, setSessionName, ...) are
        // applied rather than discarded.
        self.sync_plugin_providers(plugins).await;
        self.sync_plugin_virtual_models(plugins).await;
        self.emit_resources_discover(plugins, "startup").await;
        self.apply_extension_notifications().await;
    }

    /// Drain the actionable notifications after a run and apply them:
    /// session name persistence (+ `session_info_changed`), model switches
    /// (+ `model_select`), thinking level changes (+ `thinking_level_select`),
    /// tool partials (`tool_execution_update`), compaction requests, shutdown
    /// requests, `sendMessage`/`sendUserMessage` steer/followUp queues,
    /// `appendEntry` custom entries, `setLabel` persistence, and the
    /// `setActiveTools` override. Presentation-class notifications (status
    /// line, widgets, working message, terminal title) stay queued for the
    /// TUI drain 鈥?headless hosts never steal them, and a bounded overflow
    /// guard drops the oldest presentation notifications when no UI consumes
    /// them.
    pub(super) async fn apply_extension_notifications(&self) {
        let Some(plugins) = &self.plugins else {
            return;
        };
        // Idle hosts call this between runs: install the native session-event
        // forwarder here too, so an idle `pi.sendMessage` queue update (or a
        // manual compaction) reaches native plugins before the first prompt.
        install_native_session_event_bridge(&self.session_events, plugins);
        let notifications = plugins.take_notifications(|notification| notification.is_actionable());
        for notification in notifications {
            match notification {
                Notification::SetSessionName(name) => {
                    // Same mutator as the TUI/RPC/SDK paths: the persisted
                    // (normalized) name and BOTH `session_info_changed` faces
                    // originate there, never from this drain.
                    match self.set_session_name_opt(name.as_deref()).await {
                        Ok(final_name) => match final_name {
                            Some(name) => crate::ext_diag_info!("[ext] session name: {name}"),
                            None => crate::ext_diag_info!("[ext] session name cleared"),
                        },
                        Err(error) => {
                            crate::ext_diag!("[ext] failed to persist session name: {error}")
                        }
                    }
                }
                Notification::SetModel { provider, model_id } => {
                    let previous = self.current_model();
                    let resolved = match &provider {
                        Some(provider_id) => self
                            .registry
                            .get_model(provider_id, &model_id)
                            .or_else(|| self.registry.get_model(&previous.provider, &model_id)),
                        None => {
                            // Provider omitted: search every provider.
                            let found = self
                                .registry
                                .get_models()
                                .into_iter()
                                .find(|model| model.id == model_id);
                            Some(found.unwrap_or(previous.clone()))
                        }
                    };
                    match resolved {
                        Some(model) => {
                            crate::ext_diag_info!(
                                "[ext] model set to {}:{}",
                                model.provider,
                                model.id
                            );
                            // Same mutator as the TUI/RPC/SDK paths: the state
                            // change, the branch `model_change` entry and the
                            // extension `model_select` (source "set") all
                            // originate there. The event is not emitted here.
                            if let Err(error) = self.set_model_with_source(model, "set").await {
                                crate::ext_diag!("[ext] setModel failed: {error}");
                            }
                        }
                        None => {
                            crate::ext_diag!(
                                "[ext] setModel: unknown model `{}` (provider: {:?}); model unchanged",
                                model_id, provider
                            );
                        }
                    }
                }
                Notification::SetThinkingLevel(level) => {
                    let Some(parsed) = parse_thinking_level(&level) else {
                        crate::ext_diag!("[ext] setThinkingLevel: unknown level `{level}`");
                        continue;
                    };
                    // Same mutator as the TUI/RPC/SDK paths: clamps to the
                    // model, appends the branch entry and emits BOTH the
                    // session `thinking_level_changed` and the extension
                    // `thinking_level_select` (with `previousLevel`).
                    let applied = self.set_thinking_level(parsed).await;
                    crate::ext_diag_info!("[ext] thinking level set to {}", applied.wire_key());
                }
                Notification::ToolUpdate {
                    tool_call_id,
                    tool_name,
                    args,
                    parent_call_id,
                    partial,
                } => {
                    // The one tool-update dispatcher: the JS `ctx.toolUpdate`
                    // notification path and the native `ToolService::update`
                    // both land here, so both emit the identical payload.
                    self.emit_tool_update(
                        plugins,
                        tool_call_id,
                        tool_name,
                        args,
                        parent_call_id,
                        partial,
                    )
                    .await;
                }
                Notification::CompactRequest {
                    custom_instructions,
                } => {
                    if self.is_streaming() {
                        crate::ext_diag!("[ext] ctx.compact ignored: the agent is streaming");
                        continue;
                    }
                    if let Err(error) = self.compact(custom_instructions.as_deref()).await {
                        crate::ext_diag!("[ext] ctx.compact failed: {error}");
                    }
                }
                Notification::ShutdownRequest => {
                    // `ctx.shutdown()` is a HOST-level graceful exit, not an
                    // abort (upstream `shutdownHandler`, interactive-mode.ts:
                    // 2030-2035 / rpc-mode.ts:345-347): the run in flight (if
                    // any) settles normally and the host exits at its idle /
                    // `agent_settled` boundary. Record the request on the
                    // session so the RPC host and other headless hosts can
                    // consume it.
                    self.mark_shutdown_requested();
                    crate::ext_diag_info!(
                        "[ext] shutdown requested by an extension (ctx.shutdown)"
                    );
                }
                Notification::Abort => {
                    // `pi.abort()` / `ctx.abort()` / `ctx.shutdown()`: abort the
                    // current agent operation (upstream
                    // `ExtensionContext.abort`). This takes effect even when it
                    // arrives while a run is still starting — the abort stops
                    // that run before its first request.
                    self.abort();
                }
                Notification::SendMessage { payload } => {
                    self.consume_send_message(plugins, &payload).await;
                }
                Notification::SendUserMessage { payload } => {
                    self.consume_send_user_message(&payload).await;
                }
                Notification::AppendEntry { payload } => {
                    self.consume_append_entry(&payload).await;
                }
                Notification::SetLabel { entry_id, label } => {
                    if let Some(session) = &self.session {
                        if let Err(error) =
                            session.lock().await.set_label(&entry_id, label.as_deref())
                        {
                            crate::ext_diag!("[ext] failed to persist label: {error}");
                        }
                    } else {
                        crate::ext_diag!("[ext] setLabel ignored: the session is not persisted");
                    }
                }
                Notification::ActiveToolsOverride(names) => {
                    // Persisted in the runtime state by the host function;
                    // the merged tool registry re-filters on the next run.
                    crate::ext_diag_info!("[ext] active tools override: {} tool(s)", names.len());
                }
                other => {
                    print_extension_notification(&other);
                }
            }
        }
        // Native plugin actions (`MessageService`) queued since the last
        // drain: apply them through the SAME consume_* handlers just used for
        // the JS notifications, so run boundaries and idle drains pick them up
        // exactly like `pi.sendMessage` / `sendUserMessage` / `appendEntry`.
        // Flushed BEFORE the presentation mirror so any message event handler
        // that emits a UI notification is projected in this same drain.
        self.flush_native_actions().await;
        // `status` / `extension_ui_request` are notification-shaped in the JS
        // path; a headless session has no UI drain, so mirror them to the
        // native fan-out once (UI hosts keep their queue).
        self.mirror_presentation_notifications_to_native(plugins);
        // Bound the presentation queue for headless hosts (no TUI drain):
        // drop the oldest presentation notifications past the cap, logging
        // the printable ones.
        if plugins.pending_notification_count() > NOTIFICATION_QUEUE_CAP {
            for notification in plugins.take_notifications(|n| n.is_presentation()) {
                print_extension_notification(&notification);
            }
        }
    }

    /// Apply the actionable extension notifications queued outside a run
    /// (upstream applies a session method's side effects as the method is
    /// called). A host with an idle loop calls this between runs so an idle
    /// `pi.sendMessage(..., { triggerTurn: true })` can start its own run
    /// through [`Self::take_pending_custom_run`] /
    /// [`Self::run_custom_turn`].
    pub async fn apply_pending_extension_notifications(&self) {
        self.apply_extension_notifications().await;
    }

    /// Queue one native plugin action payload (the exact JSON shape the JS
    /// host functions produce; see `sendMessageJson` /
    /// `sendUserMessageJson` / `appendEntryJson`).
    ///
    /// Bounded at [`NATIVE_ACTION_QUEUE_CAP`]: past the cap the OLDEST action
    /// is dropped with a diagnostic, so a runaway producer can never grow the
    /// session's memory. The action is applied at the next drain
    /// ([`Self::flush_native_actions`], which `apply_extension_notifications`
    /// calls at every run boundary and idle drain).
    pub(crate) fn queue_native_action(&self, kind: NativeActionKind, payload: JsonValue) {
        let mut queue = self
            .native_actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.len() >= NATIVE_ACTION_QUEUE_CAP {
            if let Some((dropped, _)) = queue.pop_front() {
                crate::ext_diag!(
                    "[ext] native action queue full; dropped oldest {:?} action",
                    dropped
                );
            }
        }
        queue.push_back((kind, payload));
    }

    /// Drain and apply the queued native plugin actions through the SAME
    /// handlers the JS notification drain uses.
    ///
    /// A snapshot (`drain(..)`, bounded by the queue cap) is taken so an
    /// action handler that queues a follow-up action cannot extend this
    /// drain: the follow-up is applied at the next boundary, exactly like a
    /// JS handler that enqueues another notification.
    pub(crate) async fn flush_native_actions(&self) {
        // A detached MessageService flush and the host's idle drain can run
        // concurrently; the lock makes their enqueue order the apply order.
        let _flush = self.native_action_flush.lock().await;
        let actions: Vec<(NativeActionKind, JsonValue)> = self
            .native_actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect();
        for (kind, payload) in actions {
            match kind {
                NativeActionKind::SendMessage => match &self.plugins {
                    Some(plugins) => self.consume_send_message(plugins, &payload).await,
                    // Without a plugin host the message events have no
                    // consumer; the queue stays drained (nothing fabricates a
                    // delivery).
                    None => crate::ext_diag!("[ext] native sendMessage ignored: no plugin host"),
                },
                NativeActionKind::SendUserMessage => self.consume_send_user_message(&payload).await,
                NativeActionKind::AppendEntry => self.consume_append_entry(&payload).await,
            }
        }
    }

    /// Emit one `tool_execution_update` from its wire pieces — the single
    /// dispatcher behind both the JS `ctx.toolUpdate` notification arm and
    /// the native [`crate::plugins::plugin::services::ToolService::update`].
    pub(super) async fn emit_tool_update(
        &self,
        plugins: &PluginHost,
        tool_call_id: String,
        tool_name: Option<String>,
        args: Option<JsonValue>,
        parent_call_id: Option<String>,
        partial: Option<JsonValue>,
    ) {
        let mut payload = serde_json::json!({
            "type": "tool_execution_update",
            "toolCallId": tool_call_id,
            "partialResult": partial.unwrap_or(JsonValue::Null),
        });
        if let Some(tool_name) = tool_name {
            payload["toolName"] = JsonValue::String(tool_name);
        }
        if let Some(args) = args {
            payload["args"] = args;
        }
        if let Some(parent) = parent_call_id {
            payload["parentToolCallId"] = JsonValue::String(parent);
        }
        emit_plugin_event(plugins, "tool_execution_update", payload).await;
    }

    /// Native [`crate::plugins::plugin::services::ToolService::update`]: map
    /// the typed call reference onto the wire ids and dispatch through
    /// [`Self::emit_tool_update`] (the same sink the JS face reaches).
    pub(crate) async fn dispatch_native_tool_update(&self, call: ToolCallRef, partial: JsonValue) {
        let Some(plugins) = self.plugins.as_ref() else {
            return;
        };
        self.emit_tool_update(
            plugins,
            call.tool_call_id.to_string(),
            call.tool_name.as_deref().map(str::to_owned),
            call.args.as_deref().cloned(),
            call.parent_call_id.as_deref().map(str::to_owned),
            Some(partial),
        )
        .await;
    }

    /// Consume `pi.sendMessage(message, options)` (upstream
    /// `sendCustomMessage`) with its four cases:
    ///
    /// 1. `deliverAs: "nextTurn"` — held for the next run's starting message
    ///    (upstream `_pendingNextTurnMessages`).
    /// 2. The session is busy (a run is in flight OR still starting) and
    ///    `triggerTurn` is not explicitly `false` — steer (or follow-up per
    ///    `deliverAs`) into THAT run instead of starting a second one.
    /// 3. Idle with `triggerTurn: true` — the message STARTS a run
    ///    (`run_custom_turn`, drained by the host through
    ///    [`AgentSession::take_pending_custom_run`]).
    /// 4. Otherwise — append the custom message entry (while busy the append
    ///    is safe: the entry is never spliced between a tool call and its
    ///    result).
    pub(super) async fn consume_send_message(&self, plugins: &PluginHost, payload: &JsonValue) {
        let message = payload.get("message").cloned().unwrap_or(JsonValue::Null);
        let options = payload.get("options").cloned().unwrap_or(JsonValue::Null);
        let Some(record) = CustomRunMessage::from_json(&message) else {
            crate::ext_diag!("[ext] sendMessage ignored: message requires a customType");
            return;
        };
        let deliver_as = options.get("deliverAs").and_then(JsonValue::as_str);
        // An ABSENT `triggerTurn` is not `false`: while busy it still steers
        // (upstream checks `triggerTurn !== false`).
        let trigger_turn = options.get("triggerTurn").and_then(JsonValue::as_bool);

        if deliver_as == Some("nextTurn") {
            self.pending_next_turn_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(record);
            return;
        }
        if self.is_streaming() && trigger_turn != Some(false) {
            // The run in flight (or still starting) takes it: steering by
            // default, follow-up when asked. A message a `before_agent_start`
            // handler sends lands in the run that handler started preparing.
            // The queued entry carries the custom wire shape and (for a
            // stored session) persists as a custom-message entry, so
            // `customType`/`display`/`details` and image content survive.
            let prompt = self.custom_prompt_message(&record);
            if deliver_as == Some("followUp") {
                self.enqueue_follow_up(prompt);
            } else {
                self.enqueue_steering(prompt);
            }
            return;
        }
        if trigger_turn == Some(true) {
            // Idle: the message starts its own run (upstream `_startRun`).
            self.pending_custom_runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(record);
            return;
        }
        // Persist the custom message entry (upstream `_appendCustomMessage`).
        // A store-less (ephemeral) session has no session manager to append
        // to; upstream would still have one, so hold the entry in memory and
        // let the next run drain it into its first request/history instead of
        // dropping it (see `take_pending_setup_messages`).
        match &self.session {
            Some(session) => {
                if let Err(error) = session.lock().await.append_custom_message_entry(
                    &record.custom_type,
                    record.content.clone(),
                    record.display,
                    record.details.clone(),
                ) {
                    crate::ext_diag!("[ext] failed to append custom message entry: {error}");
                }
            }
            None => {
                self.pending_setup_messages
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(record.clone());
            }
        }
        let message_json = record.to_json();
        emit_plugin_event(
            plugins,
            "message_start",
            serde_json::json!({ "type": "message_start", "message": message_json }),
        )
        .await;
        emit_plugin_event(
            plugins,
            "message_end",
            serde_json::json!({ "type": "message_end", "message": message_json }),
        )
        .await;
    }

    /// Consume `pi.sendUserMessage(content, options)`: always queues a turn
    /// (upstream `sendUserMessage` → `prompt`, agent-session.ts:2429-2467).
    ///
    /// Content is a string or a `(TextContent | ImageContent)[]`. The text
    /// parts join with `\n` and the IMAGE BLOCKS ARE PRESERVED: an idle
    /// message is queued as a pending USER run
    /// ([`super::prompt_loop::PENDING_USER_RUN_TYPE`]) whose `details` carry
    /// the typed blocks, so the host pump (`take_pending_custom_run` →
    /// `run_custom_turn`) starts the run with the images attached instead of
    /// flattening them into text.
    ///
    /// `options.expandPromptTemplates` (upstream default `false`) dispatches a
    /// leading `/command` to the extension command registry first (a found
    /// command decides its own LLM interaction and the message is consumed),
    /// then expands `/skill:` and prompt templates.
    ///
    /// While the session is busy the message MUST name a delivery mode —
    /// upstream throws "Agent is already processing. Specify
    /// streamingBehavior ('steer' or 'followUp') to queue the message." The
    /// notification channel cannot reject the extension's promise, so the
    /// message is dropped with the same diagnostic instead of being queued
    /// silently.
    ///
    /// Steering/follow-up queue entries carry full model messages, so the
    /// TEXT AND IMAGE blocks of a streaming `sendUserMessage` ride one user
    /// message into the next request (upstream `AgentMessage[]` steering).
    pub(super) async fn consume_send_user_message(&self, payload: &JsonValue) {
        let content = payload.get("content").cloned().unwrap_or(JsonValue::Null);
        let options = payload.get("options").cloned().unwrap_or(JsonValue::Null);
        let deliver_as = options.get("deliverAs").and_then(JsonValue::as_str);
        let expand_prompt_templates = options
            .get("expandPromptTemplates")
            .and_then(JsonValue::as_bool)
            == Some(true);

        // Parse the content ONCE into typed blocks (upstream normalizes text
        // parts + images before `prompt`).
        let blocks = crate::session_manager::projection::content_blocks(&content);
        let mut text = blocks
            .iter()
            .filter_map(|block| match block {
                crate::ai::types::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut images: Vec<crate::ai::types::ContentBlock> = blocks
            .into_iter()
            .filter(|block| matches!(block, crate::ai::types::ContentBlock::Image(_)))
            .collect();

        // Upstream `prompt()`: an extension command executes immediately
        // (even while streaming) and consumes the message.
        if expand_prompt_templates
            && text.starts_with('/')
            && self.try_dispatch_extension_command(&text).await
        {
            return;
        }

        // The `input` event sees the unexpanded text/images (upstream runs the
        // input handlers before skill/template expansion), with source
        // `"extension"` (`sendUserMessage` → `prompt({source: "extension"})`).
        // `streamingBehavior` rides only while a run is in flight, like
        // upstream. A `handled` outcome consumes the message: no queue entry
        // and no run.
        let streaming = self.is_streaming();
        let input = match &self.plugins {
            Some(plugins) => {
                super::prompt_input::dispatch_prompt_input(
                    plugins,
                    &text,
                    images,
                    "extension",
                    if streaming { deliver_as } else { None },
                )
                .await
            }
            None => super::prompt_input::PromptInputAction::Continue(
                super::prompt_input::PromptInputTransform { text, images },
            ),
        };
        let super::prompt_input::PromptInputAction::Continue(input) = input else {
            return;
        };
        text = input.text;
        images = input.images;

        if expand_prompt_templates {
            text = self.expand_prompt_commands(&text);
        }
        if text.is_empty() && images.is_empty() {
            crate::ext_diag!("[ext] sendUserMessage ignored: empty content");
            return;
        }
        if streaming {
            if deliver_as.is_none() {
                crate::ext_diag!(
                    "[ext] sendUserMessage ignored: Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
                );
                return;
            }
            if deliver_as == Some("followUp") {
                self.enqueue_follow_up(agent_loop::PromptMessage::message(user_message(
                    &text, images,
                )));
            } else {
                self.enqueue_steering(agent_loop::PromptMessage::message(user_message(
                    &text, images,
                )));
            }
            return;
        }
        // Idle: a full user run (text + images) queued for the host pump. The
        // reserved record is decoded by `run_custom_turn` back into a user
        // starting message, so the images are not flattened. The `input`
        // event already ran above (`source: "extension"`), so the record
        // carries that fact: a source-aware pump
        // (`run_custom_turn_with_source`) consumes the prepared transform
        // instead of emitting the event a second time.
        let mut record = super::prompt_loop::pending_user_run_message(text, images);
        if let Some(details) = record.details.as_mut() {
            details["inputDispatched"] = serde_json::json!(true);
        }
        self.pending_custom_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(record);
    }

    /// Dispatch `/<name> [args]` to the plugin command registry.
    ///
    /// Returns `true` when a command with that name is registered (its outcome
    /// — success or failure — is reported here; upstream `prompt()` consumes
    /// the message either way). `false` means "not a command": the caller
    /// falls through to skill/template expansion.
    async fn try_dispatch_extension_command(&self, text: &str) -> bool {
        let Some(rest) = text.strip_prefix('/') else {
            return false;
        };
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (rest, ""),
        };
        if name.is_empty() {
            return false;
        }
        // Resolve against the unified registry (Rust backends AND JS
        // commands), the same list `PluginHost::dispatch_command` consults.
        let known = self.plugins.as_ref().is_some_and(|plugins| {
            plugins
                .registry()
                .resolved_commands()
                .iter()
                .any(|command| command.invocation_name == name || command.def.name == name)
        });
        if !known {
            return false;
        }
        match self.dispatch_plugin_command(name, args).await {
            Ok(outcome) => {
                if !outcome.ok {
                    crate::ext_diag!(
                        "[ext] sendUserMessage command /{name} failed: {}",
                        outcome.error.unwrap_or_else(|| "unknown error".to_owned())
                    );
                }
                true
            }
            Err(error) => {
                crate::ext_diag!("[ext] sendUserMessage command /{name} failed: {error}");
                true
            }
        }
    }

    /// Consume `pi.appendEntry(customType, data)`: append a custom entry to
    /// session storage.
    pub(super) async fn consume_append_entry(&self, payload: &JsonValue) {
        let Some(custom_type) = payload.get("customType").and_then(JsonValue::as_str) else {
            crate::ext_diag!("[ext] appendEntry ignored: customType is required");
            return;
        };
        let data = payload.get("data").cloned().filter(|d| !d.is_null());
        if let Some(session) = &self.session {
            // Upstream `ctx.appendEntry` persists and then emits
            // `entry_appended` for the stored entry (agent-session.ts:3465-
            // 3470); the wire entry is read back after the append so the
            // event carries the store-assigned id/parentId/timestamp.
            let appended = {
                let mut session = session.lock().await;
                match session.append_custom_entry(custom_type, data) {
                    Ok(entry_id) => session
                        .entry(&entry_id)
                        .and_then(|entry| serde_json::to_value(entry).ok()),
                    Err(error) => {
                        crate::ext_diag!("[ext] failed to append custom entry: {error}");
                        None
                    }
                }
            };
            if let Some(entry) = appended {
                emit_entry_appended_wire(self.plugins.as_deref(), &self.session_events, entry)
                    .await;
            }
        } else {
            crate::ext_diag!("[ext] appendEntry ignored: the session is not persisted");
        }
    }
}

/// Bounded presentation-notification queue for hosts with no TUI drain.
const NOTIFICATION_QUEUE_CAP: usize = 512;

/// Dispatch one persisted non-message entry's `entry_appended` event through
/// both faces (upstream `_emit({type: "entry_appended", entry})` at the
/// append sites: `ctx.appendEntry`, virtual-model state, cache-warm usage).
///
/// `entry` is the persisted session-file entry in its wire shape; the caller
/// builds it from the session manager AFTER the append (never fabricated —
/// the store assigns the id/parentId/timestamp).
pub(super) async fn emit_entry_appended_wire(
    plugins: Option<&PluginHost>,
    session_events: &Arc<std::sync::RwLock<Vec<SessionEventHandler>>>,
    entry: JsonValue,
) {
    if let Some(plugins) = plugins {
        if plugins.has_handlers("entry_appended") {
            emit_plugin_event(
                plugins,
                "entry_appended",
                serde_json::json!({
                    "type": "entry_appended",
                    "entry": entry.clone(),
                }),
            )
            .await;
        }
    }
    crate::agent_session::events::emit_session_event_to(
        session_events,
        SessionEvent::EntryAppended { entry },
    );
}

/// Dispatch one plugin event through the unified plugin host, ignoring
/// dispatch failures (events are best-effort by contract).
pub(super) async fn emit_plugin_event(
    plugins: &PluginHost,
    event: &str,
    mut payload: serde_json::Value,
) {
    if let Err(error) = plugins.emit(event, &mut payload).await {
        crate::ext_diag!("[ext] {event} dispatch failed: {error}");
    }
}

/// Report one plugin load/conflict diagnostic through the diagnostic router
/// (host-facing shape
/// mirrors the extension load-error lines).
pub(super) fn report_plugin_diagnostic(diagnostic: &crate::plugins::PluginDiagnostic) {
    match diagnostic.kind {
        crate::plugins::DiagnosticKind::Error => {
            crate::ext_diag!(
                "[ext] failed to load {}: {}",
                diagnostic.path,
                diagnostic.message
            )
        }
        crate::plugins::DiagnosticKind::Warning => {
            crate::ext_diag!(
                "[ext] warning ({}): {}",
                diagnostic.path,
                diagnostic.message
            )
        }
    }
}

/// Joined text of a custom-message content value (`"text"` string or a
/// content-block array's text parts).
pub(super) fn content_text(content: &JsonValue) -> String {
    match content {
        JsonValue::String(text) => text.clone(),
        JsonValue::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(JsonValue::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}
/// Emit the informational extension notifications through the diagnostic router.
fn print_extension_notification(notification: &Notification) {
    match notification {
        Notification::Notify { message, level } => {
            // The extension's own level decides the routing level; an
            // "error" notification keeps every occurrence (a deliberate
            // user-facing message, not a repeating failure).
            match level.as_str() {
                "error" => crate::ext_diag_fatal!("[ext] {level}: {message}"),
                "warning" | "warn" => crate::ext_diag!("[ext] {level}: {message}"),
                _ => crate::ext_diag_info!("[ext] {level}: {message}"),
            }
        }
        Notification::Status { key, text } => match text {
            Some(text) => crate::ext_diag_info!("[ext] status {key}: {text}"),
            None => crate::ext_diag_info!("[ext] status {key} cleared"),
        },
        _ => {}
    }
}
/// Public drain for the TUI host (the facade keeps its own private copy).
pub fn drain_extension_notifications_pub(runtime: &crate::extensions::ExtensionHandle) {
    drain_extension_notifications(runtime);
}
/// Emit extension side effects captured during a run as diagnostics.
///
/// `notify(message, level)` notifications use `[ext] {level}: {message}`;
/// status-line updates are diagnostics for headless hosts (the TUI drains
/// them into its status line instead).
pub(crate) fn drain_extension_notifications(runtime: &ExtensionHandle) {
    for notification in runtime.drain_notifications() {
        print_extension_notification(&notification);
    }
}
