use super::*;

impl AgentSession {
    /// Whether transport retries are enabled (upstream `autoRetryEnabled`).
    pub fn auto_retry_enabled(&self) -> bool {
        self.auto_retry.load(Ordering::SeqCst)
    }

    /// Enable or disable transport retries (upstream `setAutoRetryEnabled`).
    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        self.auto_retry.store(enabled, Ordering::SeqCst);
    }

    /// Cancel the pending retry wait (upstream `abortRetry()`): the armed
    /// `_retryAbortController` aborts, the waiting request fails with
    /// `AiError::RetryCancelled` ("Retry failed after N attempts: Retry
    /// cancelled"), and the run ends. Unlike `setAutoRetryEnabled(false)` the
    /// retry SETTING is untouched, so later requests keep retrying (upstream
    /// `abortRetry` only aborts the controller).
    pub fn abort_retry(&self) {
        self.retry_cancel.cancel();
    }

    /// Install the host's retry-notice sink (the TUI's `RetryStatusIndicator`
    /// countdown, upstream `auto_retry_start` / `auto_retry_end` consumers).
    /// Without a sink the transport keeps its own sleep (no countdown).
    pub fn set_host_retry_notice(&self, sink: RetryNoticeSink) {
        *self
            .host_retry_notice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }

    /// The retry-wait hook installed on every request's [`StreamOptions`]
    /// (upstream `_prepareRetry`'s abortable sleep + the TUI countdown): arms
    /// the per-attempt latch, notifies the host, waits out the delay and
    /// reports the outcome. `None` when no host sink is installed.
    fn retry_wait_hook(&self) -> crate::ai::types::RetryWaitHook {
        let sink = self
            .host_retry_notice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(sink) = sink else {
            return crate::ai::types::RetryWaitHook::default();
        };
        let latch = self.retry_cancel.clone();
        crate::ai::types::RetryWaitHook::new(std::sync::Arc::new(move |notice| {
            let signal = latch.arm();
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                sink(RetryEvent::Scheduled(notice));
                tokio::select! {
                    // `delay_ms` is the transport's computed backoff; `Delay`
                    // is used so a large value cannot overflow.
                    _ = tokio::time::sleep(std::time::Duration::from_millis(notice.delay_ms)) => {
                        sink(RetryEvent::Settled);
                        crate::ai::types::RetryOutcome::Proceed
                    }
                    _ = signal.cancelled() => {
                        sink(RetryEvent::Cancelled(notice));
                        crate::ai::types::RetryOutcome::Cancelled
                    }
                }
            })
        }))
    }

    /// The summarizer-retry observer for one compaction/branch-summary run
    /// (upstream `_summarizationRetryCallbacks`, agent-session.ts:3781-3804):
    /// install it around the run with
    /// [`crate::compaction::with_summarization_retry`] so the compaction
    /// module's retry transitions reach `session.subscribe` listeners as
    /// `summarization_retry_*`.
    pub(super) fn summarization_retry_observer(
        &self,
        source: super::events::SummarizationRetrySource,
    ) -> crate::compaction::SummarizationRetryObserver {
        super::events::summarization_retry_observer(&self.session_events, source)
    }

    /// The `auto_retry_start` / `auto_retry_end` observers installed on every
    /// request's [`StreamOptions`] when auto-retry is enabled (upstream
    /// `AgentSessionEvent` members, agent-session.ts:220-222): the transport's
    /// retry lifecycle is forwarded to the session-event listeners, so hosts
    /// that do not own the wait hook (the RPC session-event bridge, the TUI's
    /// background-completion channel, embedders) observe retries with the
    /// upstream event shapes. The hooks are observers — the transport's retry
    /// decisions are identical with or without them.
    fn retry_event_hooks(
        &self,
    ) -> (
        crate::ai::types::AutoRetryStartHook,
        crate::ai::types::AutoRetryEndHook,
    ) {
        let events = Arc::clone(&self.session_events);
        let start = Arc::new(move |start: &crate::ai::types::AutoRetryStart| {
            crate::agent_session::events::emit_session_event_to(
                &events,
                SessionEvent::AutoRetryStart {
                    attempt: start.attempt,
                    max_attempts: start.max_attempts,
                    delay_ms: start.delay_ms,
                    error_message: start.error_message.clone(),
                },
            );
        });
        let events = Arc::clone(&self.session_events);
        let end = Arc::new(move |end: &crate::ai::types::AutoRetryEnd| {
            crate::agent_session::events::emit_session_event_to(
                &events,
                SessionEvent::AutoRetryEnd {
                    success: end.success,
                    attempt: end.attempt,
                    final_error: end.final_error.clone(),
                },
            );
        });
        (start, end)
    }

    /// Tune transport-level retries for every request of this session
    /// (embedder seam): `max_retries` and/or `max_retry_delay_ms`; `None`
    /// fields keep the transport defaults. Applies to conversational streams
    /// and compaction completion calls alike.
    pub fn set_retry(&self, max_retries: Option<u32>, max_retry_delay_ms: Option<u64>) {
        let mut overrides = self
            .stream_overrides
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        overrides.max_retries = max_retries;
        overrides.max_retry_delay_ms = max_retry_delay_ms;
    }
    /// The request options for one request against the CURRENT model.
    pub(super) fn stream_options(&self, signal: Option<AbortSignal>) -> StreamOptions {
        self.stream_options_with_actor_hooks(&self.current_model(), signal, true)
    }

    /// Test seam: the per-request transport hooks exactly as the next
    /// request to the current model would carry them (see
    /// [`Self::stream_options`]), so a test can exercise the registered
    /// `before_provider_request` / `after_provider_response` faces without a
    /// real HTTP transport.
    #[doc(hidden)]
    pub fn provider_stream_hooks_for_test(&self) -> crate::ai::types::ProviderStreamHooks {
        self.stream_options(None).hooks
    }

    /// The request options for one request against `model`, with the transport
    /// hooks optional.
    ///
    /// `model` is the model the request actually runs under — the routed
    /// physical model under a virtual selection, or the explicit model of a
    /// compaction/summarization completion — because the credential is
    /// resolved for THAT model's provider (`AgentSession::request_api_key_for`).
    ///
    /// `actor_hooks: false` is used by the `ctx.modelRegistry.complete` host
    /// op ([`Self::complete_fn_for_host_op`]): `before_provider_request` /
    /// `after_provider_response` are awaited INLINE by the transport and
    /// their dispatch (`PluginHost::emit` → `ExtensionHandle::emit`) needs
    /// the extension actor's command loop, which is suspended inside the
    /// host op that asked for this completion — awaiting them there would
    /// wedge the actor until its call timeout. `provider_stream_event` now
    /// rides that same AWAITED path (one dispatch completes before the stream
    /// continues), so it is omitted here too: dispatching it into the actor
    /// from inside the actor would re-enter the suspended call. Upstream's
    /// `ModelRegistry.complete` options carry no `onProviderStreamEvent`
    /// either — only the agent's `streamSimple` path wires it
    /// (sdk.ts:388-426).
    pub(super) fn stream_options_with_actor_hooks(
        &self,
        model: &Model,
        signal: Option<AbortSignal>,
        actor_hooks: bool,
    ) -> StreamOptions {
        // `&self.stream_overrides` is borrowed only for the retry knobs: the
        // session identity and the transport preference are resolved (and
        // cached) through their own short-lived guard, because the id
        // resolution may have to close a superseded pooled connection.
        let (max_retries, max_retry_delay_ms) = {
            let overrides = self
                .stream_overrides
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (overrides.max_retries, overrides.max_retry_delay_ms)
        };
        let request_session_id = self.request_session_id();
        let transport_metadata = self.transport_metadata();
        // Per-request transport hooks (upstream `emitBeforeProviderRequest` /
        // `handleProviderResponse`, sdk.ts:387-396): carried on the
        // StreamOptions instead of the former process-wide transport slots, so
        // concurrent sessions never overwrite (or leak into) each other's
        // dispatch and a settled run leaves nothing behind for cache-warm /
        // compaction requests to trip over. Only populated when a handler
        // exists, so extension-free runs never touch the transport.
        let mut hooks = crate::ai::types::ProviderStreamHooks::default();
        if actor_hooks {
            if let Some(plugins) = &self.plugins {
                if plugins.has_handlers("before_provider_request") {
                    let plugins = Arc::clone(plugins);
                    hooks.before_provider_request = Some(Arc::new(move |payload: JsonValue| {
                        let plugins = Arc::clone(&plugins);
                        Box::pin(async move {
                            let mut event = serde_json::json!({
                                "type": "before_provider_request",
                                "payload": payload,
                            });
                            match plugins.emit("before_provider_request", &mut event).await {
                                Ok(outcome) => {
                                    // Upstream handlers may return a replacement
                                    // payload OR mutate `event.payload` in
                                    // place (runner.ts:1370-1393). The unified
                                    // `emit` writes every in-place mutation
                                    // (native and JS) back into `event.payload`;
                                    // the JS adapter also mirrors a returned
                                    // replacement there, but native plugins can
                                    // return one without mutating — honor the
                                    // replacement first and fall back to the
                                    // (possibly mutated) payload when the merge
                                    // produced no return value.
                                    if outcome.result.is_null() {
                                        event.get("payload").cloned().unwrap_or(JsonValue::Null)
                                    } else {
                                        outcome.result
                                    }
                                }
                                Err(error) => {
                                    crate::ext_diag!(
                                        "[ext] before_provider_request dispatch failed: {error}"
                                    );
                                    event.get("payload").cloned().unwrap_or(JsonValue::Null)
                                }
                            }
                        })
                    }));
                }
                if plugins.has_handlers("after_provider_response") {
                    let plugins = Arc::clone(plugins);
                    hooks.after_provider_response = Some(Arc::new(
                        move |status: u16, headers: std::collections::BTreeMap<String, String>| {
                            let plugins = Arc::clone(&plugins);
                            Box::pin(async move {
                                let mut event = serde_json::json!({
                                    "type": "after_provider_response",
                                    "status": status,
                                    "headers": headers,
                                });
                                if let Err(error) =
                                    plugins.emit("after_provider_response", &mut event).await
                                {
                                    crate::ext_diag!(
                                        "[ext] after_provider_response dispatch failed: {error}"
                                    );
                                }
                            })
                        },
                    ));
                }
                if plugins.has_handlers("provider_stream_event") {
                    // Per-chunk parsed provider events (upstream sdk.ts:388-401
                    // `handleProviderStreamEvent`): `{type, data, provider,
                    // api, model}`. The hook is a SYNC callback inside the
                    // provider stream; it hands the event to a dedicated
                    // dispatcher and blocks until that dispatch completed, so
                    // the stream never outruns the handler (upstream awaits
                    // inline). A slow handler throttles the provider read loop
                    // instead of accumulating events, and post-abort events
                    // are dropped (see [`ProviderStreamEventDispatcher`]).
                    hooks.on_provider_stream_event = Some(provider_stream_event_hook_with_signal(
                        plugins,
                        model,
                        signal.clone(),
                    ));
                }
            }
        }
        // The countdown/cancel seam (upstream `_prepareRetry`'s abortable
        // sleep driven by the session, `RetryStatusIndicator` in the TUI):
        // rides the per-request transport hooks. `set_auto_retry(false)` /
        // the retry setting gate it like the retry budget above. The
        // `auto_retry_start` / `auto_retry_end` observers ride along even
        // without a host sink, so the session-event stream (RPC, embedders)
        // always carries the retry lifecycle.
        if self.auto_retry.load(Ordering::SeqCst) {
            let (on_retry_start, on_retry_end) = self.retry_event_hooks();
            hooks.on_retry_start = Some(on_retry_start);
            hooks.on_retry_end = Some(on_retry_end);
            hooks.on_retry = self.retry_wait_hook();
        }
        StreamOptions {
            // The credential for the model this request runs under (not a
            // session-wide value): a `/model` switch to another provider must
            // carry THAT provider's credential.
            api_key: self.request_api_key_for(model),
            reasoning: Some(self.current_thinking()),
            signal,
            // `set_auto_retry(false)` / `abort_retry` disables transport
            // retries (upstream `autoRetryEnabled`, default true); the retry
            // knobs are otherwise the host-installed overrides.
            max_retries: if self.auto_retry.load(Ordering::SeqCst) {
                max_retries
            } else {
                Some(0)
            },
            max_retry_delay_ms,
            // The provider-facing session identity (upstream
            // `sessionManager.getSessionId()` in sdk.ts): without it the Codex
            // WebSocket continuation, the provider `prompt_cache_key` and the
            // session-affinity headers all degrade to "no session".
            session_id: request_session_id,
            metadata: transport_metadata,
            hooks,
            ..StreamOptions::default()
        }
    }

    /// The provider-facing session identity for [`StreamOptions::session_id`]
    /// (upstream `sessionManager.getSessionId()`): the persisted session id,
    /// or a UUID minted once for a store-less (`--no-session`) session so the
    /// value is stable across turns and unique per session. Cached in the
    /// per-request override cell, which is also where a storage swap closes
    /// the outgoing id's pooled Codex sockets (the swap refreshes the cell
    /// BEFORE it replaces the manager, so this resolves and caches the NEW id
    /// here).
    fn request_session_id(&self) -> Option<String> {
        let persisted = self.session_id();
        let mut overrides = self
            .stream_overrides
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Cloned out of the cell so the cached slot can be rewritten below.
        let current = overrides.request_session_id.clone();
        match (current, persisted) {
            (Some(current), Some(persisted)) if current != persisted => {
                // Session storage was replaced: the id changed, and the pool
                // is keyed by it, so the outgoing id's connections (and the
                // SSE-fallback marker) must not outlive the swap. A store-less
                // session has no swap, so its minted id stays stable.
                close_pooled_codex_sessions(current);
                overrides.request_session_id = Some(persisted.clone());
                Some(persisted)
            }
            (Some(current), _) => Some(current),
            (None, Some(persisted)) => {
                overrides.request_session_id = Some(persisted.clone());
                Some(persisted)
            }
            (None, None) => {
                let minted = uuid::Uuid::new_v4().to_string();
                overrides.request_session_id = Some(minted.clone());
                Some(minted)
            }
        }
    }

    /// The Codex transport knobs carried to the adapter through
    /// [`StreamOptions::metadata`] (the shared request options have no typed
    /// transport field; see `codex_ws::METADATA_TRANSPORT`). Resolved from the
    /// merged settings ONCE per session — upstream likewise reads
    /// `getTransport()` / `getWebSocketConnectTimeoutMs()` at SDK construction
    /// — and `None` when the settings say nothing and were never read, so a
    /// default session pays only the empty-map check downstream.
    #[cfg(feature = "provider-openai")]
    fn transport_metadata(&self) -> Option<std::collections::BTreeMap<String, JsonValue>> {
        use crate::ai::api::openai_responses::codex::codex_ws;
        let preference = {
            let mut overrides = self
                .stream_overrides
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match overrides.transport {
                Some(preference) => preference,
                None => {
                    let resolved = self.resolve_transport_preference();
                    overrides.transport = Some(resolved);
                    resolved
                }
            }
        };
        // `auto` with no connect-timeout override is the adapter's own default:
        // no metadata at all, so non-Codex adapters never see the bag.
        if preference.transport == crate::config::settings::TransportSetting::Auto
            && preference.websocket_connect_timeout_ms.is_none()
        {
            return None;
        }
        let mut metadata = std::collections::BTreeMap::new();
        metadata.insert(
            codex_ws::METADATA_TRANSPORT.to_string(),
            serde_json::to_value(preference.transport).unwrap_or(JsonValue::Null),
        );
        if let Some(ms) = preference.websocket_connect_timeout_ms {
            metadata.insert(
                codex_ws::METADATA_WEBSOCKET_CONNECT_TIMEOUT_MS.to_string(),
                JsonValue::from(ms),
            );
        }
        Some(metadata)
    }

    #[cfg(not(feature = "provider-openai"))]
    fn transport_metadata(&self) -> Option<std::collections::BTreeMap<String, JsonValue>> {
        None
    }

    /// Resolve the transport preference from the merged settings for this
    /// session's cwd and agent directory. The settings read is the same
    /// bounded load the session build uses
    /// (`runtime_loader::load_settings_for_in`), against the directory the
    /// session resolved at build (D1), not the live environment.
    #[cfg(feature = "provider-openai")]
    fn resolve_transport_preference(&self) -> TransportPreference {
        let manager = super::runtime_loader::load_settings_for_in(&self.agent_dir, &self.cwd);
        TransportPreference {
            transport: manager.transport(),
            websocket_connect_timeout_ms: manager.websocket_connect_timeout_ms(),
        }
    }

    /// Non-streaming completion callback used by compaction; drains a
    /// registry stream and resolves the final message. Uses the current
    /// session model.
    pub(super) fn complete_fn(&self) -> Arc<CompleteFn> {
        self.complete_fn_for(&self.current_model())
    }

    /// [`Self::complete_fn`] against an explicit model — compaction under a
    /// virtual selection completes against the ROUTED model (a virtual model
    /// never reaches a provider).
    ///
    /// The request carries the session's live abort signal — the run signal
    /// while a threshold/overflow compaction runs inside a prompt, or the
    /// fresh compaction signal a manual [`AgentSession::compact`] installs
    /// before summarizing. `abort()` aborts that signal, so a long
    /// summarization request is cancelled mid-stream instead of only being
    /// observed between turns.
    pub(super) fn complete_fn_for(&self, model: &Model) -> Arc<CompleteFn> {
        let signal = self
            .signal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        self.complete_fn_for_signal(model, signal)
    }

    /// [`Self::complete_fn_for`] with an explicit abort signal: `/tree`
    /// branch summarization and compaction summarization are cancellable
    /// mid-flight (upstream `GenerateBranchSummaryOptions.signal` → the
    /// summarizer stream's `options.signal`), so a cancellation reaches the
    /// in-flight provider request instead of only the next retry decision.
    pub(super) fn complete_fn_for_signal(
        &self,
        model: &Model,
        signal: AbortSignal,
    ) -> Arc<CompleteFn> {
        let options = self.stream_options_with_actor_hooks(model, Some(signal), true);
        self.complete_closure(model, options, "summarization stream did not terminate")
    }

    /// [`Self::complete_fn_for`] for the `ctx.modelRegistry.complete` host op
    /// (`__pir_host.modelCompleteJson`): same registry/model routing, but the
    /// INLINE transport hooks (`before_provider_request` /
    /// `after_provider_response` / the awaited `provider_stream_event`) are
    /// omitted — see [`Self::stream_options_with_actor_hooks`] for why
    /// dispatching into the extension actor from inside the host op would
    /// wedge it.
    pub(super) fn complete_fn_for_host_op(&self, model: &Model) -> Arc<CompleteFn> {
        let options = self.stream_options_with_actor_hooks(model, None, false);
        self.complete_closure(model, options, "completion stream did not terminate")
    }

    /// Shared tail of [`Self::complete_fn_for`] /
    /// [`Self::complete_fn_for_host_op`]: build the closure that streams
    /// through the session's provider registry. The per-chunk
    /// `provider_stream_event` hook was already installed by
    /// [`Self::stream_options_with_actor_hooks`] for the model the request
    /// runs under (and intentionally omitted on the host-op path).
    fn complete_closure(
        &self,
        model: &Model,
        options: StreamOptions,
        missing_message: &'static str,
    ) -> Arc<CompleteFn> {
        let registry = self.registry.clone();
        let model = model.clone();
        Arc::new(Box::new(move |context| {
            let registry = registry.clone();
            let model = model.clone();
            let options = options.clone();
            Box::pin(async move {
                let stream = registry.stream_resolving_auth(model, context, options);
                // Completion consumer: drain while waiting. With the bounded
                // event queue, a consumer that never pops an event parks the
                // provider producer forever (`drain_and_result` discards the
                // events this caller will never read).
                match stream.drain_and_result().await {
                    Some(message) => Ok((*message).clone()),
                    None => Err(missing_message.to_owned()),
                }
            })
        }))
    }
}

/// Close the pooled Codex WebSocket connections of a superseded session id
/// (see [`AgentSession::request_session_id`]). A session that never used the
/// Codex transport has nothing pooled under the id, so the close costs a map
/// lookup per account/url entry; without `provider-openai` there is no pool at
/// all and the id is dropped.
fn close_pooled_codex_sessions(session_id: String) {
    #[cfg(feature = "provider-openai")]
    crate::ai::api::openai_responses::codex::codex_ws::spawn_close_codex_websocket_sessions(Some(
        session_id,
    ));
    #[cfg(not(feature = "provider-openai"))]
    let _ = session_id;
}

/// The explicit API key a session was constructed with, scoped to the provider
/// it was resolved for (`AgentSessionConfig::api_key`).
///
/// A credential only ever belongs to one provider: a single session-wide key
/// applied to every request is exactly what sent the previous provider's key
/// to the model selected by `/model` (and came back as a 401). Keeping the
/// provider next to the key lets `AgentSession::request_api_key_for` apply it
/// only where it belongs.
pub(crate) struct ProviderApiKey {
    pub(crate) provider: String,
    pub(crate) key: String,
}

impl ProviderApiKey {
    pub(crate) fn new(provider: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            key: key.into(),
        }
    }
}

impl std::fmt::Debug for ProviderApiKey {
    /// Never renders the credential: `Debug` output travels into logs,
    /// diagnostics and bug reports.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderApiKey")
            .field("provider", &self.provider)
            .field("key", &"[redacted]")
            .finish()
    }
}

/// Bound on parsed provider events waiting for their extension dispatch (see
/// [`ProviderStreamEventDispatcher`]).
///
/// The sync hook hands ONE event to the dispatcher and then blocks until that
/// dispatch completed (the sync spelling of upstream's `await runner.emit`,
/// sdk.ts:388-401), so under normal single-stream use the queue never holds
/// more than the event the worker is about to take. The bound exists for the
/// rare concurrent caller of the same hook and is the "block explicitly
/// instead of dropping" contract: a full queue makes the hook wait on the
/// send, applying backpressure to the provider read loop; it never discards an
/// event.
const PROVIDER_STREAM_EVENT_BACKLOG_BOUND: usize = 1;

/// Dispatch counters of one [`ProviderStreamEventDispatcher`] (diagnostics and
/// tests read them; every field is a relaxed atomic so the hot path stays
/// lock-free).
#[derive(Default)]
struct ProviderStreamEventStats {
    /// Hook invocations (accepted and dropped).
    events: AtomicU64,
    /// Events whose `plugins.emit` completed successfully.
    dispatched: AtomicU64,
    /// Events whose `plugins.emit` returned an error (timeout, actor gone).
    failed: AtomicU64,
    /// Events skipped because the request's abort signal fired.
    dropped_aborted: AtomicU64,
    /// Events dropped because the dispatcher thread could not run / died.
    dropped_unavailable: AtomicU64,
    /// Handoffs that had to block on the bounded queue (backpressure hits).
    blocked_sends: AtomicU64,
    /// Dispatcher threads started (one per hook/request, lazily).
    worker_starts: AtomicU64,
}

/// One parsed provider event waiting for its extension dispatch.
struct ProviderStreamEventJob {
    event: JsonValue,
    /// Rendezvous ack: the worker sends it after the dispatch finished; the
    /// hook waits on it — that wait IS the awaited dispatch.
    done: std::sync::mpsc::SyncSender<()>,
}

/// Awaited per-chunk `provider_stream_event` dispatcher (one per request).
///
/// # Why a blocking handshake
///
/// Upstream's `handleProviderStreamEvent` is async and the provider stream
/// awaits it before consuming more body bytes (sdk.ts:388-401; every built-in
/// adapter does `await options.onProviderStreamEvent(...)`). In pir the hook
/// is the SYNC [`crate::ai::types::StreamHook`], invoked from inside the SSE
/// parser's feed loop in `src/ai` — a lane this change must not touch — so
/// there is no `.await` point in the hook. The only way to reproduce the
/// upstream ordering without buffering events ahead of the handler is to block
/// the caller until the dispatch completes.
///
/// The hook therefore hands the event to ONE dedicated dispatcher thread
/// through a bounded channel and blocks on the per-event ack:
///
/// - the provider stream cannot parse/read the next event before the handler
///   observed this one — exact upstream order, one dispatch in flight;
/// - a slow handler throttles the provider read loop (the queue bound is one,
///   a full queue blocks the handoff) instead of growing an unbounded
///   fire-and-forget backlog;
/// - the only drops are explicit: pre-handoff abort (upstream's stream is
///   already stopping, so no later event may reach extensions) and a
///   dispatcher that could not run, which logs a warning — never a silent
///   loss.
///
/// # Why a thread, not a task
///
/// The hook is called from the provider's async producer. In a
/// `current_thread` runtime (tests, CLI helpers, some embedders) blocking
/// that task while another task of the SAME runtime must run the dispatch
/// deadlocks the runtime. The dispatcher owns its own current-thread runtime
/// on its own OS thread, so it makes progress regardless of the caller's
/// runtime. A per-request thread is only started once the hook actually fires
/// and exits when the request (and with it the hook's sender) is dropped.
/// Each dispatch is additionally bounded by the extension host's per-call
/// timeout, so a wedged handler cannot pin the provider stream forever; the
/// failure is counted and logged, and later events keep flowing.
struct ProviderStreamEventDispatcher {
    plugins: Arc<PluginHost>,
    provider: JsonValue,
    api: JsonValue,
    model: JsonValue,
    signal: Option<AbortSignal>,
    sender: std::sync::OnceLock<std::sync::mpsc::SyncSender<ProviderStreamEventJob>>,
    /// The dispatcher reported it cannot dispatch any more (thread gone);
    /// every later event is dropped with one warning.
    closed: AtomicBool,
    warned_unavailable: AtomicBool,
    stats: Arc<ProviderStreamEventStats>,
}

impl ProviderStreamEventDispatcher {
    fn new(plugins: &Arc<PluginHost>, model: &Model, signal: Option<AbortSignal>) -> Arc<Self> {
        Arc::new(Self {
            plugins: Arc::clone(plugins),
            provider: JsonValue::String(model.provider.clone()),
            api: JsonValue::String(model.api.clone()),
            model: JsonValue::String(model.id.clone()),
            signal,
            sender: std::sync::OnceLock::new(),
            closed: AtomicBool::new(false),
            warned_unavailable: AtomicBool::new(false),
            stats: Arc::new(ProviderStreamEventStats::default()),
        })
    }

    /// The sync hook body: dispatch `data` to completion (or report why it was
    /// dropped). Called from the provider stream's parser callback.
    fn dispatch(self: &Arc<Self>, data: &JsonValue) {
        self.stats.events.fetch_add(1, Ordering::Relaxed);
        if self.signal.as_ref().is_some_and(AbortSignal::is_aborted) {
            // The request was aborted. Upstream's provider stream has already
            // stopped calling the hook, so a queued event must not reach
            // extensions; it is dropped (and counted) without blocking.
            self.stats.dropped_aborted.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.closed.load(Ordering::Relaxed) {
            self.note_unavailable();
            return;
        }
        let sender = self.sender.get_or_init(|| {
            start_provider_stream_event_worker(
                Arc::clone(&self.plugins),
                self.signal.clone(),
                Arc::clone(&self.stats),
            )
        });
        let (done, ack) = std::sync::mpsc::sync_channel(0);
        let job = ProviderStreamEventJob {
            event: self.build_event(data),
            done,
        };
        match sender.try_send(job) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(job)) => {
                // The bounded backlog is full: block until the worker takes
                // the event. Explicit backpressure, never a silent drop.
                self.stats.blocked_sends.fetch_add(1, Ordering::Relaxed);
                if sender.send(job).is_err() {
                    self.note_unavailable();
                    return;
                }
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.note_unavailable();
                return;
            }
        }
        // The sync `await`: only return once the handler dispatch completed
        // (success, failure or abort all ack). A vanished worker drops the ack
        // sender, which is reported rather than ignored.
        if ack.recv().is_err() {
            self.note_unavailable();
        }
    }

    /// The upstream event shape (`ProviderStreamEvent`, types.ts:923-930, and
    /// sdk.ts:394-400): `{type, data, provider, api, model}`.
    fn build_event(&self, data: &JsonValue) -> JsonValue {
        let mut event = serde_json::Map::with_capacity(5);
        event.insert(
            "type".to_owned(),
            JsonValue::String("provider_stream_event".to_owned()),
        );
        event.insert("data".to_owned(), data.clone());
        event.insert("provider".to_owned(), self.provider.clone());
        event.insert("api".to_owned(), self.api.clone());
        event.insert("model".to_owned(), self.model.clone());
        JsonValue::Object(event)
    }

    /// Record (and, the first time, warn about) a dispatcher that cannot take
    /// events. Later events are dropped without blocking so a dead extension
    /// runtime cannot wedge the provider stream.
    fn note_unavailable(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let dropped = self
            .stats
            .dropped_unavailable
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if !self.warned_unavailable.swap(true, Ordering::Relaxed) {
            crate::ext_diag!(
                "[ext] provider_stream_event dispatcher unavailable; dropping this and further \
                 events (dropped={dropped}, events={}, dispatched={}, failed={})",
                self.stats.events.load(Ordering::Relaxed),
                self.stats.dispatched.load(Ordering::Relaxed),
                self.stats.failed.load(Ordering::Relaxed),
            );
        }
    }
}

/// Start the dispatcher thread for one hook and return its bounded sender.
///
/// When the thread cannot start, the receiver is dropped with the failed
/// spawn closure, so every `send` fails and [`ProviderStreamEventDispatcher`]
/// reports the drop explicitly. The worker checks the request's abort signal
/// before each dispatch, so queued post-abort events are skipped instead of
/// reaching extensions.
fn start_provider_stream_event_worker(
    plugins: Arc<PluginHost>,
    signal: Option<AbortSignal>,
    stats: Arc<ProviderStreamEventStats>,
) -> std::sync::mpsc::SyncSender<ProviderStreamEventJob> {
    let (sender, receiver) = std::sync::mpsc::sync_channel::<ProviderStreamEventJob>(
        PROVIDER_STREAM_EVENT_BACKLOG_BOUND,
    );
    let worker_stats = Arc::clone(&stats);
    let spawned = std::thread::Builder::new()
        .name("pir-provider-stream-events".to_owned())
        .spawn(move || {
            // The dispatcher's own runtime: independent of the caller's (which
            // may be current_thread and therefore blocked by the hook).
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    crate::ext_diag!(
                        "[ext] provider_stream_event dispatcher runtime failed: {error}"
                    );
                    // Ack while draining so no hook waits on a channel whose
                    // worker cannot dispatch.
                    while let Ok(job) = receiver.recv() {
                        let _ = job.done.send(());
                    }
                    return;
                }
            };
            while let Ok(job) = receiver.recv() {
                if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
                    // Abort cleanup: a queued event must not reach extensions
                    // after the run was cancelled.
                    worker_stats.dropped_aborted.fetch_add(1, Ordering::Relaxed);
                    let _ = job.done.send(());
                    continue;
                }
                let mut event = job.event;
                match runtime.block_on(plugins.emit("provider_stream_event", &mut event)) {
                    Ok(_) => {
                        worker_stats.dispatched.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => {
                        worker_stats.failed.fetch_add(1, Ordering::Relaxed);
                        crate::ext_diag!("[ext] provider_stream_event dispatch failed: {error}");
                    }
                }
                // Always ack, including after a handler error: the run
                // continues, and the next event may still be dispatched.
                let _ = job.done.send(());
            }
        });
    match spawned {
        Ok(_) => {
            stats.worker_starts.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => {
            crate::ext_diag!(
                "[ext] provider_stream_event dispatcher thread failed to start: {error}"
            );
            // The failed spawn dropped the receiver; the sender below stays
            // closed, so the hook reports explicit drops.
        }
    }
    sender
}

/// Build the awaited per-chunk `on_provider_stream_event` hook (see
/// [`ProviderStreamEventDispatcher`]). No abort signal: the print host's hook
/// builder (which owns the run's cancellation outside this module) calls this
/// signature; the session path uses
/// [`provider_stream_event_hook_with_signal`].
#[cfg(feature = "print")]
pub(crate) fn provider_stream_event_hook(
    plugins: &Arc<PluginHost>,
    model: &Model,
) -> crate::ai::types::StreamHook {
    provider_stream_event_hook_with_signal(plugins, model, None)
}

/// [`provider_stream_event_hook`] bound to the request's abort signal, so an
/// abort stops delivery of later parsed events (the upstream stream stops
/// consuming).
fn provider_stream_event_hook_with_signal(
    plugins: &Arc<PluginHost>,
    model: &Model,
    signal: Option<AbortSignal>,
) -> crate::ai::types::StreamHook {
    let dispatcher = ProviderStreamEventDispatcher::new(plugins, model, signal);
    Arc::new(move |data: &JsonValue| dispatcher.dispatch(data))
}
