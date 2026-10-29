use super::*;

use crate::session_manager::json_depth;
use rquickjs::Object;

// `getSessionManagerJson`: the runtime state already holds every field
// serialized (and the tree as pre-rendered TEXT, see
// `crate::extensions::SessionContextSnapshot::tree_json`), so the reply is
// hand-assembled in the closure below: `serde_json` encodes each shallow field
// and the tree text is spliced in place. The previous struct-serializer form
// could not stay: a nested `Value` tree recurses one serializer frame — and
// one drop-glue frame — per entry. Field order matches the old literal
// (`serde_json` runs with `preserve_order`): `entries`, `sessions`,
// `sessionId`, `sessionPath`, `cwd`, `header`, `branch`, `tree`, `leafId`,
// `labels`, `projection`.

/// `getSystemPromptOptionsJson` envelope (upstream `BuildSystemPromptOptions`).
/// Borrowed so the custom prompt is written straight out of the runtime state
/// instead of being copied into a `serde_json::Map` (a full copy of the
/// prompt) only to be stringified. Key order matches the old insertion order
/// (`preserve_order`): `cwd`, then `customPrompt` only when non-empty.
#[derive(serde::Serialize)]
struct SystemPromptOptionsJson<'a> {
    cwd: &'a str,
    #[serde(rename = "customPrompt", skip_serializing_if = "Option::is_none")]
    custom_prompt: Option<&'a str>,
}

/// `modelRegistry.complete` success envelope `{ok: true, message}`. Borrowed:
/// the assistant message is serialized from the typed value instead of being
/// cloned into a `Value` (`to_value`) and then cloned a second time by the
/// `json!` envelope. Field order matches the old literal.
#[derive(serde::Serialize)]
struct CompleteOk<'a, M: serde::Serialize> {
    ok: bool,
    message: &'a M,
}

pub(super) fn register_context_host_fns(
    host: &Object,
    state: &Arc<StdMutex<RuntimeState>>,
) -> rquickjs::Result<()> {
    macro_rules! sync_fn {
        ($key:literal, $closure:expr) => {
            host.set($key, Func::from($closure))?;
        };
    }

    {
        // Upstream `ExtensionContext.isIdle` (types.ts:348): "Whether the
        // agent is idle (not streaming)". Reads the LIVE session-pushed
        // streaming cell — the same source `waitForIdle` uses — keyed by THIS
        // runtime's state Arc (per-runtime: concurrent runtimes in one
        // process observe only their own streaming state). The
        // construction-time `config.is_idle` freezes at startup and never
        // observes run transitions; hosts that never push streaming state see
        // the cell's idle default.
        let st = state.clone();
        sync_fn!("isIdle", move || -> bool {
            !crate::extensions::handle::session_stream_snapshot(
                crate::extensions::handle::session_stream_key(&st),
            )
            .0
        });
    }
    // -- ctx.waitForIdle (upstream ExtensionCommandContext.waitForIdle,
    //    types.ts:405-406) ----------------------------------------------------
    //
    // Resolves as soon as no prompt run is in flight. The live flag is the
    // session-pushed streaming cell for THIS runtime
    // ([`crate::extensions::handle::set_session_streaming`]). The wait parks
    // on the cell's notify handle AND re-checks the flag on a short timer
    // interval (first wake wins): the host future suspends inside the
    // QuickJS job executor, where an interleaved `async_with` (an eval or
    // renderer call while the wait is pending) can lose the parked notify
    // wakeup — the timer path guarantees the flag is re-observed. Requires
    // the runtime's executor drive loop
    // ([`ExtensionRuntime::spawn_drive_loop`]) so suspended futures are
    // re-polled across interleaved calls. Deadline-bounded (30 min, the
    // reserved-hook window): a wedged run never wedges the awaiting JS
    // command forever.
    {
        let st = state.clone();
        host.set(
            "waitForIdle",
            Func::from(JsAsync(move || {
                // Clone per call: the async block needs its own `st` (the
                // closure itself must stay `Fn`).
                let st = st.clone();
                async move {
                    use crate::extensions::handle::{session_stream_key, session_stream_snapshot};
                    const WAIT_FOR_IDLE_TIMEOUT: std::time::Duration =
                        std::time::Duration::from_secs(30 * 60);
                    const WAIT_FOR_IDLE_POLL: std::time::Duration =
                        std::time::Duration::from_millis(50);
                    let deadline = tokio::time::Instant::now() + WAIT_FOR_IDLE_TIMEOUT;
                    let key = session_stream_key(&st);
                    loop {
                        let (streaming, notify) = session_stream_snapshot(key);
                        if !streaming {
                            return "{}".to_string();
                        }
                        let notified = notify.notified();
                        // Re-check after registering the waiter (the flag may
                        // clear between the snapshot and the wait).
                        if !session_stream_snapshot(key).0 {
                            return "{}".to_string();
                        }
                        tokio::select! {
                            // The cell's owner pushed the idle transition.
                            _ = notified => {}
                            // Timer path: bounds any wakeup lost to the
                            // interleaved-with-suspension executor behavior.
                            _ = tokio::time::sleep(WAIT_FOR_IDLE_POLL) => {}
                            _ = tokio::time::sleep_until(deadline) => {
                                return r#"{"error":"waitForIdle timed out"}"#.to_string();
                            }
                        }
                    }
                }
            })),
        )?;
    }
    // -- timer primitive (Node-parity shim foundation) ------------------------
    //
    // Upstream extensions run in Node, where `setTimeout`/`setInterval` are
    // process globals. pir embeds QuickJS on the extension actor thread, so the
    // JS timer surface (prelude) is built on this host sleep: the actor's
    // tokio current-thread executor has timers enabled, and an awaited sleep
    // future stays driven under the pending JS call exactly like `uiDialog`
    // waits. The deadline is Node's `setTimeout` cap (2^31-1 ms, timers.js).
    {
        host.set(
            "sleep",
            Func::from(JsAsync(move |ms: f64| async move {
                let ms = if ms.is_finite() && ms > 0.0 {
                    ms.min((1_u64 << 31) as f64 - 1.0)
                } else {
                    0.0
                };
                tokio::time::sleep(std::time::Duration::from_millis(ms as u64)).await;
                "ok".to_string()
            })),
        )?;
    }
    {
        let st = state.clone();
        sync_fn!("getSystemPrompt", move || -> String {
            lock_state(&st).config.system_prompt.clone()
        });
    }
    {
        let st = state.clone();
        sync_fn!("getSystemPromptOptionsJson", move || -> String {
            let s = lock_state(&st);
            // The session host pushes the normalized `BuildSystemPromptOptions`
            // object it derived the live prompt from
            // (`AgentSession::system_prompt_options_snapshot`, upstream
            // `_baseSystemPromptOptions`). Before the first prompt (a
            // factory-time `ctx.getSystemPromptOptions()`), fall back to the
            // minimal shape backed by real state: the custom prompt override
            // and the working directory.
            if s.system_prompt_options.is_object() {
                return s.system_prompt_options.to_string();
            }
            let cwd = s.config.cwd.display().to_string();
            let custom = s.config.system_prompt.as_str();
            serde_json::to_string(&SystemPromptOptionsJson {
                cwd: &cwd,
                custom_prompt: if custom.is_empty() {
                    None
                } else {
                    Some(custom)
                },
            })
            .unwrap_or_else(|_| "{}".to_string())
        });
    }
    // Render a `BuildSystemPromptOptions` object with the ordered section
    // builder (upstream `buildSystemPrompt`, system-prompt.ts:208): the JS
    // `before_agent_start` event re-renders `event.systemPrompt` from the
    // mutable `systemPromptOptions` after every handler, so a handler that
    // mutates `sections`/`selectedTools`/`skills` is observed by later
    // handlers exactly like upstream's getter (runner.ts:1427-1470).
    sync_fn!(
        "renderSystemPromptOptionsJson",
        move |options_json: String| -> String {
            let options: JsonValue = serde_json::from_str(&options_json).unwrap_or(JsonValue::Null);
            if !options.is_object() {
                return String::new();
            }
            let package_dir = crate::config::env::package_dir();
            crate::extensions::system_prompt_options::render_json(&options, package_dir.as_deref())
                .unwrap_or_default()
        }
    );
    // Chunk sink for a running custom `BashOperations.exec`
    // (`user_bash` `{operations}` result): the JS `onData` callback pushes each
    // chunk here synchronously while the executor call is suspended. A plain
    // process-wide sink lookup — no actor re-entry, no runtime-state lock.
    sync_fn!("bashOperationData", move |token: String,
                                        data: String|
          -> () {
        crate::extensions::user_bash::deliver_chunk(&token, &data);
    });
    sync_fn!("getAgentDir", move || -> String {
        pir_agent_dir().display().to_string()
    });

    // -- ctx facade ----------------------------------------------------------

    {
        let st = state.clone();
        sync_fn!("getSessionManagerJson", move || -> String {
            let s = lock_state(&st);
            let session = &s.session;
            // Hand-assembled text in the field order the old struct
            // serializer wrote (under `preserve_order`), so the tree can be
            // SPLICED as pre-rendered text instead of being held as a nested
            // `Value`: a session tree nests one level per entry, and
            // `serde_json` recurses per level both when it serializes — and
            // when it drops — such a value.
            let mut out = String::with_capacity(1024 + session.tree_json.len());
            out.push('{');
            let mut first = true;
            json_depth::push_field(&mut out, &mut first, "entries", &session.entries);
            json_depth::push_field(&mut out, &mut first, "sessions", &session.sessions);
            json_depth::push_field(&mut out, &mut first, "sessionId", &session.session_id);
            json_depth::push_field(&mut out, &mut first, "sessionPath", &session.session_path);
            json_depth::push_field(&mut out, &mut first, "cwd", &session.cwd);
            json_depth::push_field(&mut out, &mut first, "header", &session.header);
            json_depth::push_field(&mut out, &mut first, "branch", &session.branch);
            // Seven fields precede the tree, so a separator is always due here.
            out.push(',');
            out.push_str("\"tree\":");
            if session.tree_json.is_empty() {
                // No session attached (model-only snapshot): the field was
                // `Null` before the text representation.
                out.push_str("null");
            } else {
                out.push_str(&session.tree_json);
            }
            json_depth::push_field(&mut out, &mut first, "leafId", &session.leaf_id);
            json_depth::push_field(&mut out, &mut first, "labels", &session.labels);
            json_depth::push_field(&mut out, &mut first, "projection", &session.projection);
            out.push('}');
            out
        });
    }
    // -- scalar `ctx.sessionManager` accessors --------------------------------
    //
    // `getSessionManagerJson` hand-writes the WHOLE session envelope (context
    // projection, full tree text, per-entry projection copies, session
    // listing). The prelude used to parse it just to read one scalar field
    // (`getSessionId` / `getLeafId` / `getCwd` / `getLabel` / ...). These host
    // functions read the same runtime state directly, so the scalar getters
    // cost one lookup instead of a snapshot-sized serialize + JS parse. The
    // big JSON stays the source for the collection getters (`getEntries`,
    // `getTree`, `getBranch`, `buildContextEntries`, `buildSessionProjection`)
    // and `getEntry`/`getLeafEntry`.
    {
        let st = state.clone();
        sync_fn!("getSessionIdJson", move || -> Option<String> {
            lock_state(&st).session.session_id.clone()
        });
    }
    {
        let st = state.clone();
        sync_fn!("getSessionPathJson", move || -> Option<String> {
            lock_state(&st).session.session_path.clone()
        });
    }
    {
        let st = state.clone();
        sync_fn!("getSessionCwdJson", move || -> Option<String> {
            lock_state(&st).session.cwd.clone()
        });
    }
    {
        let st = state.clone();
        sync_fn!("getSessionLeafIdJson", move || -> Option<String> {
            lock_state(&st).session.leaf_id.clone()
        });
    }
    {
        let st = state.clone();
        sync_fn!("getSessionHeaderJson", move || -> String {
            // Same text `getSessionManagerJson` splices for the `header` field
            // (`null` for an absent header, the caller maps that to
            // `undefined` like the former `parsed.header ?? undefined`).
            serde_json::to_string(&lock_state(&st).session.header)
                .unwrap_or_else(|_| "null".to_string())
        });
    }
    {
        let st = state.clone();
        sync_fn!(
            "getSessionLabelJson",
            move |entry_id: String| -> Option<String> {
                // Mirrors `(parsed.labels ?? {})[entry_id]`: a missing label map
                // (`Null`) and an unlabeled entry both read as `undefined` on the
                // JS side.
                lock_state(&st)
                    .session
                    .labels
                    .get(entry_id.as_str())
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned)
            }
        );
    }
    {
        let st = state.clone();
        sync_fn!("getModelsJson", move || -> String {
            lock_state(&st).model_registry.to_string()
        });
    }
    {
        let st = state.clone();
        sync_fn!("getScopedModelsJson", move || -> String {
            lock_state(&st).scoped_models.to_string()
        });
    }
    {
        let st = state.clone();
        sync_fn!("getContextUsageJson", move || -> String {
            let s = lock_state(&st);
            let session = &s.session;
            let usage = match (session.context_tokens, session.context_window) {
                (Some(tokens), Some(window)) if window > 0 => serde_json::json!({
                    "tokens": tokens,
                    "contextWindow": window,
                    "percent": (tokens as f64 / window as f64) * 100.0,
                }),
                (None, Some(window)) => serde_json::json!({
                    "tokens": null,
                    "contextWindow": window,
                    "percent": null,
                }),
                _ => serde_json::Value::Null,
            };
            serde_json::json!({ "usage": usage }).to_string()
        });
    }
    {
        let st = state.clone();
        sync_fn!("hasPendingMessages", move || -> bool {
            lock_state(&st).session.has_pending_messages
        });
    }
    {
        let st = state.clone();
        sync_fn!("isProjectTrusted", move || -> bool {
            lock_state(&st).session.project_trusted
        });
    }
    {
        let st = state.clone();
        sync_fn!("getThinkingLevel", move || -> String {
            lock_state(&st).thinking_level.clone()
        });
    }
    {
        let st = state.clone();
        sync_fn!("setThinkingLevel", move |level: String| -> () {
            let mut s = lock_state(&st);
            s.thinking_level = level.clone();
            s.push(Notification::SetThinkingLevel(level));
        });
    }
    {
        let st = state.clone();
        sync_fn!("setModel", move |provider: Option<String>,
                                   model_id: String|
              -> bool {
            let mut s = lock_state(&st);
            // Upstream returns false when the provider has no usable
            // credential and leaves the model unchanged. The authenticated
            // list is host-pushed; an empty list means the host cannot check
            // (permissive), a non-empty list is authoritative.
            if let Some(provider) = provider.as_deref().filter(|p| !p.is_empty()) {
                if !s.authenticated_providers.is_empty()
                    && !s.authenticated_providers.iter().any(|id| id == provider)
                {
                    return false;
                }
            }
            // The bridge owns both arguments and nothing reads them after the
            // push, so move them into the notification instead of the clone
            // pair the old form paid.
            s.push(Notification::SetModel { provider, model_id });
            true
        });
    }
    {
        let st = state.clone();
        sync_fn!(
            "hasAuthenticatedProvider",
            move |provider: String| -> bool {
                let s = lock_state(&st);
                // Backs `ctx.modelRegistry.hasConfiguredAuth(model)` (upstream
                // ModelRegistry.hasConfiguredAuth, model-registry.ts:76-78). Same
                // host-pushed authenticated-provider list and permissive-empty
                // convention as `setModel`: an empty list means the host cannot
                // check, a non-empty list is authoritative.
                s.authenticated_providers.is_empty()
                    || s.authenticated_providers.iter().any(|id| id == &provider)
            }
        );
    }
    {
        let st = state.clone();
        sync_fn!("requestCompact", move |custom_instructions: Option<
            String,
        >|
              -> bool {
            // Whether the host will even attempt the compaction: the
            // notification consumer ignores the request while a prompt run is
            // in flight (upstream manual `compact()` aborts/waits instead;
            // pir's notification seam logs and drops). Reporting it lets the
            // JS `ctx.compact` callbacks settle with an error immediately
            // instead of waiting for a compaction that never starts.
            let streaming = crate::extensions::handle::session_stream_snapshot(
                crate::extensions::handle::session_stream_key(&st),
            )
            .0;
            lock_state(&st).push(Notification::CompactRequest {
                custom_instructions,
            });
            !streaming
        });
    }
    {
        let st = state.clone();
        sync_fn!("shutdown", move || -> () {
            // Upstream `ExtensionContext.shutdown` (`agent-session.ts:3510-
            // 3512`) routes to the host's shutdown handler, never to the
            // abort path: the TUI marks `shutdownRequested` and exits when
            // idle (or at `agent_settled`), RPC marks its flag and exits
            // after the settling run / next command. A run in flight settles
            // normally.
            lock_state(&st).push(Notification::ShutdownRequest);
        });
    }

    // -- registerProvider / unregisterProvider --------------------------------

    {
        let st = state.clone();
        sync_fn!("registerProviderJson", move |json: String| -> () {
            if let Ok(def) = serde_json::from_str::<JsonValue>(&json) {
                let mut s = lock_state(&st);
                let id = def.get("id").and_then(JsonValue::as_str).map(String::from);
                if let Some(id) = id {
                    s.provider_defs.retain(|existing| {
                        existing.get("id").and_then(JsonValue::as_str) != Some(id.as_str())
                    });
                    s.provider_defs.push(def);
                    // Definition-class write outside the export registry: only
                    // the host gate's generation moves (`registry_gen` would
                    // buy a full `__pir_exportRegistryJson` re-export that this
                    // class never appears in).
                    s.defs_gen = s.defs_gen.wrapping_add(1);
                }
            }
        });
    }
    {
        let st = state.clone();
        sync_fn!("unregisterProvider", move |name: String| -> () {
            let mut s = lock_state(&st);
            s.provider_defs.retain(|existing| {
                existing.get("id").and_then(JsonValue::as_str) != Some(name.as_str())
            });
            s.defs_gen = s.defs_gen.wrapping_add(1);
        });
    }

    // -- registerMcpServer / unregisterMcpServer / getMcpServers --------------
    //
    // The core only stores the registrations (upstream `McpServerRegistry`);
    // the `mcp` builtin picks them up at the next registry rebuild and emits
    // `mcp_servers_change`.

    {
        let st = state.clone();
        sync_fn!("registerMcpServerJson", move |json: String| -> () {
            if let Ok(def) = serde_json::from_str::<JsonValue>(&json) {
                let mut s = lock_state(&st);
                let name = def
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .map(String::from);
                if let Some(name) = name {
                    s.mcp_server_defs.retain(|existing| {
                        existing.get("name").and_then(JsonValue::as_str) != Some(name.as_str())
                    });
                    s.mcp_server_defs.push(def);
                    // Definition-class write outside the export registry: move
                    // the host gate's generation only (see
                    // `registerProviderJson`).
                    s.defs_gen = s.defs_gen.wrapping_add(1);
                }
            }
        });
    }
    // -- registerVirtualModel / unregisterVirtualModel / getPhysicalModel -----
    //
    // Virtual models (upstream `registerVirtualModel`): definitions accumulate
    // in runtime state tagged with their plugin source; the session host
    // applies them to the provider registry's virtual-model overlay at the
    // next prompt. `getPhysicalModel` reads the host-pushed model registry
    // snapshot.

    {
        let st = state.clone();
        sync_fn!("registerVirtualModelJson", move |json: String| -> () {
            if let Ok(def) = serde_json::from_str::<JsonValue>(&json) {
                let mut s = lock_state(&st);
                // The upstream JS form is `{id, physical: {provider, id}, ...}`;
                // a top-level `provider` overrides the physical provider.
                // Borrowed from `def` (moved into the list only after the
                // retain below), so the old `to_owned()` pair copied both ids
                // for nothing.
                let provider = def
                    .get("provider")
                    .and_then(JsonValue::as_str)
                    .or_else(|| {
                        def.get("physical")
                            .and_then(|physical| physical.get("provider"))
                            .and_then(JsonValue::as_str)
                    })
                    .unwrap_or_default();
                let id = def
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                if provider.is_empty() || id.is_empty() {
                    return;
                }
                s.virtual_model_defs.retain(|existing| {
                    let existing_provider = existing
                        .get("provider")
                        .and_then(JsonValue::as_str)
                        .or_else(|| {
                            existing
                                .get("physical")
                                .and_then(|physical| physical.get("provider"))
                                .and_then(JsonValue::as_str)
                        })
                        .unwrap_or_default();
                    existing.get("id").and_then(JsonValue::as_str) != Some(id)
                        || existing_provider != provider
                });
                s.virtual_model_defs.push(def);
                // Definition-class write outside the export registry: move
                // the host gate's generation only (see
                // `registerProviderJson`).
                s.defs_gen = s.defs_gen.wrapping_add(1);
            }
        });
    }
    {
        let st = state.clone();
        sync_fn!("unregisterMcpServer", move |name: String| -> () {
            let mut s = lock_state(&st);
            s.mcp_server_defs.retain(|existing| {
                existing.get("name").and_then(JsonValue::as_str) != Some(name.as_str())
            });
            s.defs_gen = s.defs_gen.wrapping_add(1);
        });
    }
    {
        let st = state.clone();
        sync_fn!("unregisterVirtualModel", move |provider: String,
                                                 id: String|
              -> () {
            // `RuntimeState::remove_virtual_model` bumps `defs_gen` itself
            // (the one helper both this host fn and the handle accessor use),
            // so the definition write is covered at the mutation site.
            lock_state(&st).remove_virtual_model(&provider, &id);
        });
    }
    {
        let st = state.clone();
        sync_fn!("getMcpServersJson", move || -> String {
            let s = lock_state(&st);
            // Serialize the stored list in place: wrapping it in
            // `JsonValue::Array(..)` first deep-copied every definition before
            // stringifying it. A `Vec<JsonValue>` serializes as the same array.
            serde_json::to_string(&s.mcp_server_defs).unwrap_or_else(|_| "[]".to_string())
        });
    }
    {
        let st = state.clone();
        sync_fn!("getPhysicalModelJson", move |provider: String,
                                               id: String|
              -> Option<String> {
            lock_state(&st)
                .get_physical_model(&provider, &id)
                .and_then(|model| serde_json::to_string(&model).ok())
        });
    }

    // -- ctx.modelRegistry.complete host path ---------------------------------
    //
    // `modelRegistry.complete` (upstream ModelRegistry.complete,
    // model-registry.ts:142-148) runs the provider's api implementation. For
    // providers the extension runtime cannot execute itself (builtin catalog /
    // models.json providers with no extension `streamSimple` callback) the JS
    // side calls this op; the session host pushed
    // `AgentSession::complete_fn_for(current model)` into the runtime state
    // (`ExtensionHandle::set_complete_fn`), which streams through the
    // session's provider registry with the session request options (api key,
    // thinking level, retry knobs, provider hooks). The op answers
    // `{ok: true, message: AssistantMessage}` or `{ok: false, error}` as JSON;
    // without a pushed capability it fails closed with a clear error instead
    // of fabricating a message.
    {
        let st = state.clone();
        host.set(
            "modelCompleteJson",
            Func::from(JsAsync(
                move |model_json: String, context_json: String, options_json: String| {
                    let st = st.clone();
                    async move {
                        // `options` is accepted for upstream shape
                        // compatibility (`complete(model, context, options?)`).
                        // The pushed closure owns the request options, so only
                        // the context is applied here; a per-call override would
                        // need a `CompleteFn` that takes options (the seam does
                        // not carry them).
                        let _ = options_json;
                        let capability = { lock_state(&st).host_completion.clone() };
                        let Some(capability) = capability else {
                            return serde_json::json!({
                                "ok": false,
                                "error": "modelRegistry.complete: the pir host has no completion \
                                          capability in this mode (no session model runtime attached)",
                            })
                            .to_string();
                        };
                        let model: crate::ai::types::Model = match serde_json::from_str(&model_json)
                        {
                            Ok(model) => model,
                            Err(error) => {
                                return serde_json::json!({
                                    "ok": false,
                                    "error": format!("modelRegistry.complete: invalid model: {error}"),
                                })
                                .to_string()
                            }
                        };
                        let context: crate::ai::types::Context =
                            match serde_json::from_str(&context_json) {
                                Ok(context) => context,
                                Err(error) => {
                                    return serde_json::json!({
                                        "ok": false,
                                        "error": format!("modelRegistry.complete: invalid context: {error}"),
                                    })
                                    .to_string()
                                }
                            };
                        // The closure was built for the session's current model;
                        // completing under any other model would silently run the
                        // wrong request, so it fails closed with a clear error.
                        if model.provider != capability.provider || model.id != capability.model_id {
                            return serde_json::json!({
                                "ok": false,
                                "error": format!(
                                    "modelRegistry.complete: the host completion is bound to the \
                                     session model {}/{}; requesting {}/{} is not supported by \
                                     this host op",
                                    capability.provider,
                                    capability.model_id,
                                    model.provider,
                                    model.id,
                                ),
                            })
                            .to_string();
                        }
                        match (*capability.complete)(context).await {
                            // Infallible: `CompleteOk` is a `bool` plus the
                            // `AssistantMessage` tree, whose every field is
                            // plain data (`String`/ints/floats/`bool`/
                            // `Option`/`Vec`/`BTreeMap<String, _>`/
                            // `serde_json::Value`/unit enums) behind derived
                            // `Serialize` impls — no non-string map key, no
                            // custom impl, in-memory writer. `expect` removes
                            // the unreachable `Err` branch (whose text was a
                            // `serde_json` serializer artifact, not a real
                            // outcome) instead of a dead fallback.
                            Ok(message) => serde_json::to_string(&CompleteOk {
                                ok: true,
                                message: &message,
                            })
                            .unwrap_or_else(|error| {
                                serde_json::json!({
                                    "ok": false,
                                    "error": format!(
                                        "modelRegistry.complete: serialization failed: {error}"
                                    ),
                                })
                                .to_string()
                            }),
                            Err(error) => serde_json::json!({
                                "ok": false,
                                "error": format!("modelRegistry.complete: {error}"),
                            })
                            .to_string(),
                        }
                    }
                },
            )),
        )?;
    }

    // -- `PI_TIMING=1` per-handler attribution ------------------------------
    //
    // The JS dispatch loop times each handler invocation (including awaits)
    // and reports the elapsed time with the extension id, event name and
    // success flag. The identity resolved here is SANITIZED (the last
    // path/package components, never a full home-directory path); the event
    // name is reduced to a short identifier. No prompt, payload or secret
    // crosses this boundary. The gate is checked on BOTH sides so a disabled
    // run pays one boolean read and no host round-trip per handler.
    {
        sync_fn!("timingEnabled", || -> bool {
            crate::config::timings::timing_enabled_cached()
        });
    }
    {
        let st = state.clone();
        sync_fn!("handlerTiming", move |ext_id: f64,
                                        event: String,
                                        elapsed_ms: f64,
                                        ok: bool|
              -> () {
            if !crate::config::timings::timing_enabled_cached() {
                return;
            }
            let ext_id = if ext_id.is_finite() && ext_id >= 0.0 {
                ext_id as u64
            } else {
                0
            };
            let identity = lock_state(&st)
                .ext_identities
                .get(&ext_id)
                .cloned()
                .unwrap_or_else(|| format!("ext#{ext_id}"));
            let event = sanitized_event_label(&event);
            let elapsed = if elapsed_ms.is_finite() && elapsed_ms > 0.0 {
                elapsed_ms as u64
            } else {
                0
            };
            eprintln!(
                "pir-timing: ext.handler.{event}: {elapsed}ms plugin={identity} status={}",
                if ok { "ok" } else { "error" }
            );
        });
    }
    // Dispatch-epoch tracking for the abandoned-dispatch rule: the prelude
    // pushes the epoch around every handler invocation (its `await`s
    // included) and pops it when the handler settles. A continuation resuming
    // after the caller stopped waiting therefore still tags its side effects
    // with the abandoned epoch, and `RuntimeState::push` drops them.
    {
        let st = state.clone();
        sync_fn!("enterDispatch", move |epoch: f64| -> () {
            if !epoch.is_finite() || epoch <= 0.0 {
                return;
            }
            let mut state = lock_state(&st);
            // Bound: a handler torn down by an interrupt never runs its JS
            // `finally`, so an entry can outlive its dispatch. Only the
            // innermost entries matter for tagging; evict the oldest.
            const MAX_TRACKED_DISPATCHES: usize = 256;
            if state.js_dispatch_epochs.len() >= MAX_TRACKED_DISPATCHES {
                state.js_dispatch_epochs.remove(0);
            }
            state.js_dispatch_epochs.push(epoch as u64);
        });
    }
    {
        let st = state.clone();
        sync_fn!("exitDispatch", move |epoch: f64| -> () {
            if !epoch.is_finite() || epoch <= 0.0 {
                return;
            }
            lock_state(&st).exit_js_dispatch(epoch as u64);
        });
    }

    Ok(())
}

/// Short, log-safe label for an event name: ASCII alphanumerics, `_`, `-`
/// and `.` survive (max 48 chars); anything else becomes `other`. Event names
/// are registered by extensions, so they must never splice arbitrary text
/// into a timing line.
fn sanitized_event_label(event: &str) -> String {
    const MAX: usize = 48;
    if event.is_empty()
        || event.len() > MAX
        || !event
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return "other".to_owned();
    }
    event.to_owned()
}
