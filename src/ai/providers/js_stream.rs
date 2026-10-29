//! JS-callback streaming provider (upstream `registerProvider({streamSimple})`)
//! plus the JS bridges shared with the virtual-model `route(request)` callback.
//!
//! Rust equivalent of upstream `packages/ai` `streamSimple` registration: an
//! extension hands pir a JS callback that, per request, produces the assistant
//! event stream. pir keeps the callback inside QuickJS (functions cannot cross
//! the JSON host-fn bridge) and invokes it through a dedicated streaming actor
//! command (`Command::CallHookStream` →
//! [`crate::extensions::ExtensionHandle::call_hook_stream`] →
//! [`crate::extensions::runtime::ExtensionRuntime::call_hook_stream`]): the
//! runtime starts the JS callback (`__pir_startProviderStream`) and then pulls
//! the produced events one at a time (`__pir_streamNext`) through an mpsc
//! channel, bridged here into a live
//! [`AssistantMessageEventStream`]. Reserved-prefix hook calls run under the
//! long [`crate::extensions::runtime::HOOK_CALL_TIMEOUT`] (30 minutes, per
//! round trip — in incremental mode that is the maximum gap between two
//! events) instead of the 30s default per-call timeout, the runtime maps a
//! JSON `{error}` reply to a fail-closed terminal, and a dropped events
//! channel (abort / consumer teardown) closes the JS session.
//!
//! Wire contract (superset of upstream):
//! - `streamSimple(model, context, options)` returns (or resolves to):
//!   - an array of [`AssistantMessageEvent`] wire objects (legacy batch form),
//!   - `{events: [...]}` (legacy batch form), or
//!   - an async iterable of events (upstream's live
//!     `AssistantMessageEventStream` shape: `[Symbol.asyncIterator]`, or a raw
//!     `{next()}` async iterator; `{events: <iterable>}` also accepted).
//!     pir pulls it incrementally; consumers observe each event as it is
//!     produced.
//! - `route(request)` (upstream `VirtualModelDefinition.route`) returns
//!   `{model: {provider, id}, thinkingLevel?, state?}`; failures fall back to
//!   the static route (`crate::ai::virtual`). `state` is the sticky router
//!   state (upstream `ModelRoute.state`): the bridge keeps the last returned
//!   state per registration and feeds it back as `request.state` on the next
//!   route.
//!
//! Failure is fail-closed: a throwing/absent callback, an actor/timeout error,
//! or a malformed event never panics and never blocks the extension actor —
//! the request terminates with a typed `Error` assistant event.

use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::ai::auth::{ApiKeyAuth, AuthContext};
use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::provider::{Provider, ProviderBox, ProviderRegistry};
use crate::ai::types::{
    AssistantMessageEvent, AuthResult, Context, ErrorReason, Model, ModelAuth, StreamOptions,
};
use crate::extensions::runtime::HookStreamItem;
use crate::extensions::ExtensionHandle;

/// Reserved `call_prepare_loadout` key prefix routing to the streamSimple
/// dispatcher (`__pir_stream:<providerId>`). Tool names are validated
/// identifiers, so a real tool can never collide with the prefix.
pub const JS_STREAM_HOOK_PREFIX: &str = "__pir_stream:";

/// Reserved `call_prepare_loadout` key prefix routing to the virtual-model
/// route dispatcher (`__pir_route:<provider>/<id>`).
pub const JS_ROUTE_HOOK_PREFIX: &str = "__pir_route:";

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

/// A provider whose streaming is produced by an extension's JS
/// `streamSimple` callback (upstream `registerProvider` with `streamSimple`).
/// Model listing is static (the registered `models` config), like
/// `providers/dynamic.rs`.
pub struct JsStreamProvider {
    provider_id: String,
    display_name: String,
    models: Vec<Model>,
    js: ExtensionHandle,
}

impl JsStreamProvider {
    /// Create a provider from already-normalized models (provider id/base url
    /// stamped by [`RegisterJsStreamProviderShared::register_js_stream_provider_shared`]).
    pub fn new(provider_id: impl Into<String>, models: Vec<Model>, js: ExtensionHandle) -> Self {
        let provider_id = provider_id.into();
        let display_name = provider_id.clone();
        Self {
            provider_id,
            display_name,
            models,
            js,
        }
    }

    /// The request payload handed to the JS callback (upstream's
    /// `(model, context, options)` arguments, JSON-wire form). `hook_session`
    /// (when hooks are bound) makes the prelude install callable
    /// `onPayload`/`onResponse`/`onProviderStreamEvent` wrappers in
    /// `options` (upstream `SimpleStreamOptions` hook passthrough).
    fn request_payload(
        model: &Model,
        context: &Context,
        options: &StreamOptions,
        hook_session: Option<&str>,
    ) -> JsonValue {
        serde_json::json!({
            "model": serde_json::to_value(model).unwrap_or(JsonValue::Null),
            "systemPrompt": context.system_prompt,
            "messages": serde_json::to_value(&context.messages).unwrap_or(JsonValue::Null),
            "tools": context
                .tools
                .as_ref()
                .and_then(|tools| serde_json::to_value(tools).ok()),
            "options": {
                "api": model.api,
                "baseUrl": model.base_url,
                "apiKey": options.api_key,
                "thinking": options
                    .reasoning
                    .map(|level| level.to_string()),
                "hookSession": hook_session,
            },
        })
    }
}

impl Provider for JsStreamProvider {
    fn id(&self) -> &str {
        &self.provider_id
    }

    fn name(&self) -> &str {
        &self.display_name
    }

    fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }

    /// Keyless-friendly auth (upstream `streamSimple` providers own their
    /// credentials inside the JS callback): the resolver always succeeds with
    /// an empty auth so `ProviderRegistry::stream`'s configured-gate never
    /// rejects a JS-stream provider. Explicit `StreamOptions.api_key` still
    /// flows through to the callback payload.
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![
            ApiKeyAuth::new("Extension streamSimple provider", Vec::new())
                .with_federation(js_stream_always_authenticated),
        ]
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.stream_shared(model, Arc::new(context), options)
    }

    /// The request payload is serialized from the shared transcript directly,
    /// so the provider never needs an owned `Context` copy first.
    fn stream_shared(
        &self,
        model: &Model,
        context: Arc<Context>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let output = AssistantMessageEventStream::new();
        if options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
        {
            output.push(AssistantMessageEventStream::error_event(
                ErrorReason::Aborted,
                crate::ai::provider::error_message(model, "Request aborted"),
            ));
            return output;
        }
        // Options hooks (upstream `SimpleStreamOptions.onPayload` /
        // `onResponse` / `onProviderStreamEvent`): when the caller supplied
        // any, bind them under a per-request session id so the prelude can
        // install callable wrappers in the callback's `options` object; the
        // binding is released when the stream ends (the guard below).
        let js = self.js.clone();
        let hooks = options.hooks.clone();
        let has_hooks = hooks.on_payload.is_some()
            || hooks.on_response.is_some()
            || hooks.on_provider_stream_event.is_some();
        let hook_session = has_hooks.then(|| {
            static NEXT_HOOK_SESSION: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            format!(
                "pir-stream-hooks-{}",
                NEXT_HOOK_SESSION.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            )
        });
        if let Some(session) = &hook_session {
            js.bind_stream_hooks(
                session,
                crate::extensions::runtime::StreamHookBridge {
                    on_payload: hooks.on_payload,
                    on_response: hooks.on_response,
                    on_provider_stream_event: hooks.on_provider_stream_event,
                },
            );
        }
        let payload =
            Self::request_payload(model, context.as_ref(), &options, hook_session.as_deref());
        let hook_key = format!("{JS_STREAM_HOOK_PREFIX}{}", self.provider_id);
        let model = model.clone();
        let signal = options.signal.clone();
        // Producer handle, not a consumer clone: `output` is returned to the
        // caller as THE consumer, so the forwarding task must not also count as
        // one — otherwise a consumer that abandons the stream is unobservable
        // and `run_js_stream` would park in `push_await` forever.
        let stream_out = output.producer();
        tokio::spawn(async move {
            // Releases the hook binding on every exit path.
            let _hook_guard = HookBindingGuard {
                js: js.clone(),
                session: hook_session,
            };
            run_js_stream(js, hook_key, payload, model, stream_out, signal).await;
        });
        output
    }
}

/// Releases the streamSimple options-hook binding when the stream task ends
/// (any exit path — clean finish, typed error, or abort).
struct HookBindingGuard {
    js: ExtensionHandle,
    session: Option<String>,
}

impl Drop for HookBindingGuard {
    fn drop(&mut self) {
        if let Some(session) = &self.session {
            self.js.unbind_stream_hooks(session);
        }
    }
}

/// Always-resolving federation (see [`Provider::api_key_auth`]).
fn js_stream_always_authenticated(_ctx: &dyn AuthContext) -> AiResult<Option<AuthResult>> {
    Ok(Some(AuthResult {
        auth: ModelAuth {
            api_key: None,
            headers: None,
            base_url: None,
        },
        env: None,
        source: Some("extension streamSimple provider".to_string()),
    }))
}

/// Drive one JS streamSimple round trip and forward the produced events into
/// `output` incrementally. Every failure path terminates with a typed `Error`
/// event; none panics or leaves the stream unterminated. An aborted
/// `signal` terminates with the typed aborted error (upstream's aborted
/// stream semantics): the events channel is dropped, which stops the runtime
/// pull loop and closes the JS-side session.
async fn run_js_stream(
    js: ExtensionHandle,
    hook_key: String,
    payload: JsonValue,
    model: Model,
    output: AssistantMessageEventStream,
    signal: Option<crate::ai::types::AbortSignal>,
) {
    let fail = |message: String| {
        output.push(AssistantMessageEventStream::error_event(
            ErrorReason::Error,
            crate::ai::provider::error_message(&model, &message),
        ));
    };
    let fail_aborted = || {
        output.push(AssistantMessageEventStream::error_event(
            ErrorReason::Aborted,
            crate::ai::provider::error_message(&model, "Request aborted"),
        ));
    };
    let mut events = js.call_hook_stream(&hook_key, &payload).await;
    loop {
        let item = if let Some(signal) = signal.as_ref() {
            tokio::select! {
                // Abort wins over in-flight events: dropping the receiver
                // stops the runtime pull loop and closes the JS session.
                biased;
                _ = signal.cancelled() => {
                    drop(events);
                    fail_aborted();
                    return;
                }
                item = events.recv() => match item {
                    Some(item) => item,
                    None => break,
                },
            }
        } else {
            match events.recv().await {
                Some(item) => item,
                None => break,
            }
        };
        match item {
            HookStreamItem::Event(raw) => {
                match serde_json::from_value::<AssistantMessageEvent>(raw) {
                    // Producer backpressure: the JS pull loop (and the
                    // extension runtime) waits for queue space instead of the
                    // stream retaining snapshots without bound. `false` means
                    // the consumer is gone: stop pulling (dropping `events`
                    // closes the JS-side session) instead of parking forever.
                    Ok(event) => {
                        if !output.push_await(event).await {
                            drop(events);
                            return;
                        }
                    }
                    Err(error) => {
                        fail(format!(
                            "streamSimple callback produced an invalid event: {error}"
                        ));
                        return;
                    }
                }
            }
            HookStreamItem::End => break,
            HookStreamItem::Failed(message) => {
                // A host abort reaches the JS side through the runtime abort
                // flag (`__pir_streamNext` replies `{aborted: true}`); when
                // the signal is aborted the terminal is the typed aborted
                // error, matching the local-abort path above.
                if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    fail_aborted();
                } else {
                    fail(message);
                }
                return;
            }
        }
    }
    if !output.is_done() {
        fail("streamSimple callback produced no terminal event".to_string());
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Shared (interior-mutable) registration of a [`JsStreamProvider`] for an
/// `Arc<ProviderRegistry>` — the seam the session host's `registerProvider`
/// sync uses (upstream `registerProvider({streamSimple})`). Mirrors
/// `providers/dynamic.rs`'s shared overlay: dynamic entries win over static
/// providers on every lookup.
pub trait RegisterJsStreamProviderShared {
    /// Register (or replace) a JS-stream provider serving `models`.
    /// Provider ids/base urls are stamped onto the models; at least one model
    /// is required (upstream's streamSimple providers declare their models).
    fn register_js_stream_provider_shared(
        &self,
        provider_id: impl Into<String>,
        models: Vec<Model>,
        js: ExtensionHandle,
    ) -> AiResult<ProviderBox>;
}

impl RegisterJsStreamProviderShared for ProviderRegistry {
    fn register_js_stream_provider_shared(
        &self,
        provider_id: impl Into<String>,
        models: Vec<Model>,
        js: ExtensionHandle,
    ) -> AiResult<ProviderBox> {
        let provider_id = provider_id.into();
        let models: Vec<Model> = models
            .into_iter()
            .map(|mut model| {
                model.provider = provider_id.clone();
                if model.base_url.is_empty() {
                    // Metadata only; JS-stream providers never hit the url.
                    model.base_url = "extension://stream".to_string();
                }
                model
            })
            .collect();
        if models.is_empty() {
            return Err(AiError::UnknownProvider(format!(
                "streamSimple provider {provider_id} has no models"
            )));
        }
        let provider: ProviderBox =
            Arc::new(JsStreamProvider::new(provider_id.clone(), models, js));
        self.dynamic
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider_id, provider.clone());
        Ok(provider)
    }
}

// ---------------------------------------------------------------------------
// Virtual-model route callback bridge (upstream `route(request)`)
// ---------------------------------------------------------------------------

/// Build the per-request route callback for one virtual model: each call
/// round-trips to the JS `route(request)` hook through the reserved
/// `__pir_route:` channel. `Err` (JS throw, timeout, dead actor, malformed
/// decision) makes the caller fall back to the static route — see
/// [`crate::ai::virtual::VirtualModelRegistry::route_callback`].
///
/// Sticky router state (upstream `ModelRoute.state`, stored on the session
/// branch under `pi.virtual-model-state`): the bridge keeps the last
/// decision state per registration in an in-memory cell, injects it into the
/// serialized request as `request.state` (upstream `ModelRouteRequest.state`)
/// and overwrites it only when the callback returns a new state (returning
/// `undefined` keeps the current one — upstream semantics). The surrogate is
/// in-memory: it lives as long as the installed callback, not across resume.
pub fn js_virtual_route_callback(
    js: ExtensionHandle,
    provider: String,
    id: String,
) -> crate::ai::virtual_::VirtualRouteFn {
    let sticky_state: Arc<std::sync::Mutex<Option<JsonValue>>> = Arc::default();
    Arc::new(move |request: crate::ai::virtual_::VirtualRouteRequest| {
        let js = js.clone();
        let hook_key = format!("{JS_ROUTE_HOOK_PREFIX}{provider}/{id}");
        let sticky_state = sticky_state.clone();
        Box::pin(async move {
            let mut payload = serde_json::to_value(&request)
                .map_err(|error| format!("route request serialization failed: {error}"))?;
            if let Some(state) = sticky_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                if let Some(object) = payload.as_object_mut() {
                    object.insert("state".to_owned(), state);
                }
            }
            let response = js
                .call_prepare_loadout(&hook_key, &payload)
                .await
                .map_err(|error| format!("route dispatch failed: {error}"))?;
            let Some(response) = response else {
                return Err("route callback is not registered".to_string());
            };
            if let Some(error) = response.get("error").and_then(JsonValue::as_str) {
                return Err(error.to_string());
            }
            let decision: crate::ai::virtual_::VirtualRouteDecision =
                serde_json::from_value(response).map_err(|error| {
                    format!("route callback returned an invalid decision: {error}")
                })?;
            // Upstream: a returned state is stored (returning the previous
            // state or undefined keeps the current one), then fed back on the
            // next route.
            if let Some(state) = &decision.state {
                *sticky_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state.clone());
            }
            Ok(decision)
        })
    })
}

// ---------------------------------------------------------------------------
// JS-callback OAuth flow (upstream `ProviderConfig.oauth`, types.ts 1943-1958)
// ---------------------------------------------------------------------------
//
// An extension registers its own OAuth login flow (`login(callbacks)`,
// `refreshToken(credentials, signal)`, `getApiKey(credentials)`) for /login
// support. The functions cannot cross the JSON host-fn bridge, so they stay in
// QuickJS and the host drives them through the detached start/poll protocol
// installed by the prelude (`__pir_providerOauthStart/Poll/Cancel`):
//
// - `login`: the JS side runs the stored callback with JS-implemented
//   `OAuthLoginCallbacks`; notify-style callbacks (onAuth / onDeviceCode /
//   onProgress) land in the op's event log and are forwarded here to the
//   login UI's `AuthInteraction`, while prompt-style callbacks surface through
//   the existing `uiDialog` channel (kind `input` / `select`). The op resolves
//   with `{credential, apiKey}`.
// - `refresh`: one detached op carrying the stored credential.
// - `to_auth` is sync and performs no I/O: the JS wrapper derives the API key
//   at login/refresh time, and the bridge stores it in the credential's
//   `apiKey` extra field (preserved verbatim on the auth.json wire).
//
// Every op is deadline-bounded on the Rust side and deterministically
// cancellable (`__pir_providerOauthCancel`); a JS op may only ever await host
// fns (bounded by the dialog TTL), so an abandoned op cannot wedge the actor.

/// Overall bound for one interactive `login` round trip (user prompts ride the
/// dialog TTL per step, but the whole flow must terminate).
pub const OAUTH_LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Bound for one `refreshToken` round trip.
pub const OAUTH_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Detached-op poll cadence (each poll is one cheap `eval_json` round trip;
/// the extension runtime's resident executor drive loop
/// (`ExtensionRuntime::spawn_drive_loop`) keeps the pending JS continuations
/// running between polls — the actor's post-command `drain_pending_jobs`
/// sweep only catches jobs left queued when a command task finished first).
const OAUTH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// An extension-registered OAuth flow executed by JS callbacks (upstream
/// `adaptOAuth` in provider-composer.ts).
pub struct JsOAuthFlow {
    provider_id: String,
    display_name: String,
    is_subscription: bool,
    js: ExtensionHandle,
}

impl JsOAuthFlow {
    pub fn new(
        provider_id: impl Into<String>,
        display_name: impl Into<String>,
        is_subscription: bool,
        js: ExtensionHandle,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            display_name: display_name.into(),
            is_subscription,
            js,
        }
    }

    /// Run one detached JS OAuth op to completion, forwarding each notify
    /// event through `on_event`. Returns the op's `{ok, credential, apiKey}`
    /// result.
    async fn run_op(
        &self,
        op: &str,
        payload: &JsonValue,
        timeout: std::time::Duration,
        mut on_event: impl FnMut(&JsonValue),
    ) -> AiResult<JsonValue> {
        static NEXT_OAUTH_OP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let call_id = format!(
            "pir-oauth-{}",
            NEXT_OAUTH_OP.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        // The prelude's `__pir_providerOauthStart(providerId, op, payloadJson,
        // callId)` runs `parseHostJson(payloadJson)`, so the payload must
        // arrive as a JSON *string* literal (an embedded object literal would
        // reach JSON.parse as "[object Object]" and parse to undefined).
        let payload_text =
            serde_json::to_string(payload).map_err(|error| AiError::Http(error.to_string()))?;
        let payload_arg = serde_json::to_string(&payload_text)
            .map_err(|error| AiError::Http(error.to_string()))?;
        let start_code = format!(
            "globalThis.__pir_providerOauthStart({}, {}, {}, {})",
            serde_json::to_string(&self.provider_id)
                .map_err(|error| AiError::Http(error.to_string()))?,
            serde_json::to_string(op).map_err(|error| AiError::Http(error.to_string()))?,
            payload_arg,
            serde_json::to_string(&call_id).map_err(|error| AiError::Http(error.to_string()))?,
        );
        self.js
            .eval_json(&start_code)
            .await
            .map_err(|error| AiError::Http(format!("oauth op dispatch failed: {error}")))?;
        let deadline = tokio::time::Instant::now() + timeout;
        let poll_code = format!("globalThis.__pir_providerOauthPoll({call_id:?})");
        loop {
            if tokio::time::Instant::now() >= deadline {
                // Deterministic give-up: mark the op abandoned; its remaining
                // JS awaits are bounded by the dialog TTL.
                let _ = self
                    .js
                    .eval_json(&format!(
                        "globalThis.__pir_providerOauthCancel({call_id:?})"
                    ))
                    .await;
                return Err(AiError::Http(format!(
                    "provider `{}` oauth {op} timed out",
                    self.provider_id
                )));
            }
            tokio::time::sleep(OAUTH_POLL_INTERVAL).await;
            let reply = self
                .js
                .eval_json(&poll_code)
                .await
                .map_err(|error| AiError::Http(format!("oauth op poll failed: {error}")))?;
            for event in reply
                .get("events")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                on_event(event);
            }
            if reply.get("done").and_then(JsonValue::as_bool) == Some(true) {
                return Ok(reply.get("result").cloned().unwrap_or(JsonValue::Null));
            }
        }
    }
}

/// Map one JS oauth event-log entry to the login UI's `AuthEvent` (upstream
/// `OAuthLoginCallbacks.onAuth/onDeviceCode/onProgress` → `AuthEvent`).
fn auth_event_from_json(event: &JsonValue) -> Option<crate::ai::auth::AuthEvent> {
    use crate::ai::auth::AuthEvent;
    match event.get("type").and_then(JsonValue::as_str)? {
        "auth_url" => Some(AuthEvent::AuthUrl {
            url: event
                .get("url")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string(),
            instructions: event
                .get("instructions")
                .and_then(JsonValue::as_str)
                .map(String::from),
        }),
        "device_code" => Some(AuthEvent::DeviceCode {
            user_code: event
                .get("userCode")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string(),
            verification_uri: event
                .get("verificationUri")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string(),
            interval_seconds: event.get("intervalSeconds").and_then(JsonValue::as_u64),
            expires_in_seconds: event.get("expiresInSeconds").and_then(JsonValue::as_u64),
        }),
        "progress" => Some(AuthEvent::Progress {
            message: event
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        _ => None,
    }
}

impl crate::ai::auth::OAuthFlow for JsOAuthFlow {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn is_subscription(&self) -> bool {
        self.is_subscription
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn crate::ai::auth::AuthInteraction,
        _options: &'a crate::ai::auth::LoginOptions,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = AiResult<crate::ai::auth::OAuthCredential>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let result = self
                .run_op("login", &JsonValue::Null, OAUTH_LOGIN_TIMEOUT, |event| {
                    if let Some(auth_event) = auth_event_from_json(event) {
                        interaction.notify(auth_event);
                    }
                })
                .await?;
            if result.get("ok").and_then(JsonValue::as_bool) != Some(true) {
                let error = result
                    .get("error")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("login failed");
                return Err(AiError::NonRetryable(error.to_string()));
            }
            let mut credential =
                parse_oauth_credential(&self.provider_id, result.get("credential"))?;
            // The JS op resolves `{ok, credential, apiKey}`: the derived key
            // rides at the RESULT level, not inside the credential object.
            if let Some(api_key) = result.get("apiKey").and_then(JsonValue::as_str) {
                credential.set_extra("apiKey", Some(JsonValue::String(api_key.to_string())));
            }
            Ok(credential)
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a crate::ai::auth::OAuthCredential,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = AiResult<crate::ai::auth::OAuthCredential>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let payload = serde_json::json!({ "credential": credential });
            let result = self
                .run_op("refresh", &payload, OAUTH_REFRESH_TIMEOUT, |_| {})
                .await?;
            if result.get("ok").and_then(JsonValue::as_bool) != Some(true) {
                let error = result
                    .get("error")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("token refresh failed");
                return Err(AiError::NonRetryable(error.to_string()));
            }
            let mut credential =
                parse_oauth_credential(&self.provider_id, result.get("credential"))?;
            if let Some(api_key) = result.get("apiKey").and_then(JsonValue::as_str) {
                credential.set_extra("apiKey", Some(JsonValue::String(api_key.to_string())));
            }
            Ok(credential)
        })
    }

    fn to_auth(&self, credential: &crate::ai::auth::OAuthCredential) -> AiResult<ModelAuth> {
        // The JS wrapper derives the API key at login/refresh time and the
        // bridge stored it in the credential's `apiKey` extra field; to_auth
        // is a pure projection (upstream `toAuth` → `{apiKey}`).
        match credential.extra_str("apiKey") {
            Some(api_key) if !api_key.is_empty() => Ok(ModelAuth {
                api_key: Some(api_key.to_string()),
                headers: None,
                base_url: None,
            }),
            _ => Err(AiError::NonRetryable(format!(
                "provider `{}`: the stored oauth credential carries no derived API key",
                self.provider_id
            ))),
        }
    }
}

/// Parse the JS credential object into an [`crate::ai::auth::OAuthCredential`]
/// (upstream `OAuthCredentials`: `{type: "oauth", refresh, access, expires,
/// ...}` — unknown fields flatten into `extra` and survive the auth.json wire)
/// and stamp the derived API key into the `apiKey` extra field.
fn parse_oauth_credential(
    provider_id: &str,
    value: Option<&JsonValue>,
) -> AiResult<crate::ai::auth::OAuthCredential> {
    let mut credential: crate::ai::auth::OAuthCredential =
        serde_json::from_value(value.cloned().unwrap_or(JsonValue::Null)).map_err(|error| {
            AiError::NonRetryable(format!(
                "provider `{provider_id}` oauth credential is invalid: {error}"
            ))
        })?;
    if let Some(api_key) = value
        .and_then(|value| value.get("apiKey"))
        .and_then(JsonValue::as_str)
    {
        credential.set_extra("apiKey", Some(JsonValue::String(api_key.to_string())));
    }
    Ok(credential)
}

// ---------------------------------------------------------------------------
// OAuth-flow overlay (upstream composeOAuthAuth attaching the extension flow)
// ---------------------------------------------------------------------------

/// A provider wrapper that overrides only `oauth_flow`, delegating everything
/// else to the wrapped provider. The session host applies it after registering
/// an extension provider whose config carries oauth hooks, so `/login` and the
/// auth-status surfaces discover the extension's flow through the standard
/// [`Provider::oauth_flow`] seam.
struct OAuthFlowOverlay {
    inner: ProviderBox,
    flow: crate::ai::auth::OAuthFlowBox,
}

impl Provider for OAuthFlowOverlay {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn get_models(&self) -> Vec<Model> {
        self.inner.get_models()
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.inner.stream(model, context, options)
    }

    /// Delegate the shared transcript handle straight through; the overlay
    /// only substitutes [`Provider::oauth_flow`], so it must not reintroduce
    /// the owned-context copy the wrapped provider already avoids.
    fn stream_shared(
        &self,
        model: &Model,
        context: Arc<Context>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.inner.stream_shared(model, context, options)
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        self.inner.api_key_auth()
    }

    fn oauth_flow(&self) -> Option<crate::ai::auth::OAuthFlowBox> {
        Some(self.flow.clone())
    }
}

/// Wrap `inner` so [`Provider::oauth_flow`] reports `flow` (everything else
/// delegates).
pub fn with_oauth_flow(inner: ProviderBox, flow: crate::ai::auth::OAuthFlowBox) -> ProviderBox {
    Arc::new(OAuthFlowOverlay { inner, flow })
}
