use super::*;

/// Minimum interval between two `provider:refreshModels` dispatches for the
/// same provider definition. The hook performs a network refresh (and a model
/// store write), and `sync_plugin_providers` runs on every prompt; without
/// this gate every prompt paid one refresh round trip even though the model
/// list changes on the order of hours. A changed definition (different JSON
/// fingerprint) bypasses the window.
const REFRESH_MODELS_WINDOW: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Last refresh of one provider id: the definition fingerprint it was
/// refreshed for, when that happened, and the model list the refresh returned
/// (the wire `models` array, `None` when the refresh produced no usable list).
///
/// The cache is scoped to one session's `ProviderRegistry` (its pointer is the
/// map key): every session has its own registry and its own JS refresh
/// callbacks, so a second session must dispatch again and receive that
/// session's `context.stored`, even when the provider id and definition are
/// identical. Sharing the stamp process-wide leaked one session's refresh into
/// a concurrent session (and skipped the callback entirely).
struct RefreshStamp {
    fingerprint: u64,
    last_refresh: std::time::Instant,
    models: Option<JsonValue>,
}

fn refresh_stamps() -> &'static std::sync::Mutex<std::collections::HashMap<String, RefreshStamp>> {
    static STAMPS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, RefreshStamp>>,
    > = std::sync::OnceLock::new();
    STAMPS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn refresh_stamp_key(scope: usize, id: &str) -> String {
    format!("{scope}:{id}")
}

/// Fingerprint of one provider definition: any change to the definition (new
/// models, baseUrl, auth, a replaced callback marker) must re-run the refresh.
fn provider_config_fingerprint(config: &JsonValue) -> u64 {
    use std::hash::{Hash as _, Hasher as _};

    let text = serde_json::to_string(config).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Whether `id`'s `refreshModels` hook should run now: first sight, a changed
/// definition, or a window older than [`REFRESH_MODELS_WINDOW`].
fn refresh_models_due(scope: usize, id: &str, fingerprint: u64) -> bool {
    let now = std::time::Instant::now();
    let stamps = refresh_stamps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match stamps.get(&refresh_stamp_key(scope, id)) {
        Some(stamp) => {
            stamp.fingerprint != fingerprint
                || now.duration_since(stamp.last_refresh) >= REFRESH_MODELS_WINDOW
        }
        None => true,
    }
}

fn mark_refresh_models(scope: usize, id: &str, fingerprint: u64, models: Option<JsonValue>) {
    refresh_stamps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            refresh_stamp_key(scope, id),
            RefreshStamp {
                fingerprint,
                last_refresh: std::time::Instant::now(),
                models,
            },
        );
}

/// The last successful refresh's model list for `id`, when the stamp belongs
/// to the same definition. `None` after a failed/empty refresh (or a changed
/// definition), so the caller falls back to the persisted catalog / registry.
fn cached_refresh_models(scope: usize, id: &str, fingerprint: u64) -> Option<JsonValue> {
    refresh_stamps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&refresh_stamp_key(scope, id))
        .filter(|stamp| stamp.fingerprint == fingerprint)
        .and_then(|stamp| stamp.models.clone())
}

/// Parse a wire `models` array against the provider's `baseUrl`/`api` (the
/// same shape the refresh result is parsed with). `None` when the list has no
/// usable entries.
fn parse_refreshed_models(config: &JsonValue, models: &JsonValue) -> Option<Vec<Model>> {
    let refreshed_config = serde_json::json!({
        "baseUrl": config.get("baseUrl").cloned().unwrap_or(JsonValue::Null),
        "api": config.get("api").cloned().unwrap_or(JsonValue::Null),
        "models": models,
    });
    parse_provider_models(&refreshed_config)
        .ok()
        .filter(|models| !models.is_empty())
}

impl AgentSession {
    /// Convert `pi.registerProvider` definitions 鈥?from the unified plugin
    /// registry (JS extensions and Rust builtins alike) 鈥?into real registry
    /// providers. Idempotent: definitions are upserted by provider id each
    /// time. Async: a `refreshModels` hook (upstream
    /// `ProviderConfig.refreshModels`) refreshes the model list through a
    /// bounded JS round trip at registration.
    pub(super) async fn sync_plugin_providers(&self, plugins: &PluginHost) {
        for def in plugins.registry().provider_defs() {
            let Some(id) = def.get("id").and_then(JsonValue::as_str) else {
                continue;
            };
            let config = def.get("config").cloned().unwrap_or(JsonValue::Null);
            // Provider-level `headers`/`authHeader` (upstream ProviderConfig
            // headers/authHeader, #22): provider headers merge under per-model
            // headers (model entries win), and `authHeader: true` stamps
            // `Authorization: Bearer <key>` with the resolved API key
            // (upstream withConfiguredAuth). The merge runs AFTER the refresh
            // below: `refreshModels` replaces the model list wholesale, so
            // merging only the configured entries would drop the provider's
            // Authorization/custom headers from every refreshed model.
            let provider_headers = apply_provider_headers(&config);
            let auth_header = config
                .get("authHeader")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            let mut models = match parse_provider_models(&config) {
                Ok(models) => models,
                Err(error) => {
                    crate::ext_diag!("[ext] provider `{id}` skipped: {error}");
                    continue;
                }
            };
            let resolved_key = config
                .get("apiKey")
                .and_then(JsonValue::as_str)
                .map(interpolate_api_key)
                .filter(|key| !key.is_empty());
            if auth_header && resolved_key.is_none() {
                crate::ext_diag!(
                    "[ext] provider `{id}`: authHeader requires a resolved API key; requests will fail auth"
                );
            }
            // `refreshModels: true` marker (upstream
            // `ProviderConfig.refreshModels`, types.ts:1941): the registered
            // JS callback refreshes the model list through the
            // `provider:refreshModels` synthetic event; a non-empty result
            // replaces the configured models (upstream refreshModels
            // semantics).
            if config.get("refreshModels").and_then(JsonValue::as_bool) == Some(true) {
                let fingerprint = provider_config_fingerprint(&config);
                let stamp_scope = self.registry.refresh_scope_id() as usize;
                if !refresh_models_due(stamp_scope, id, fingerprint) {
                    // Inside the window with an unchanged definition: do not
                    // pay the network refresh again, and do not overwrite the
                    // previously refreshed list with the configured (stale)
                    // one. The cached list is the refresh's own outcome; a
                    // session whose registry was never populated (a second
                    // session in this process sharing the stamp) restores it
                    // instead of falling through to the configured models.
                    let cached = cached_refresh_models(stamp_scope, id, fingerprint)
                        .and_then(|models| parse_refreshed_models(&config, &models));
                    let restored = cached.or_else(|| {
                        read_provider_catalog(self, id)
                            .and_then(|entry| restore_models_from_store(&config, id, &entry))
                            .or_else(|| {
                                let existing = self.registry.get_provider_models(id);
                                (!existing.is_empty()).then_some(existing)
                            })
                    });
                    if let Some(restored) = restored {
                        models = restored;
                    }
                } else {
                    // The persisted catalog is the provider-scoped snapshot the
                    // refresh callback sees as `context.stored` (upstream
                    // `ModelsStore.read` -> `RefreshModelsContext.stored`). It is
                    // also the cross-restart fallback when the refresh yields no
                    // usable models (offline start / callback failure).
                    //
                    // The wire copy exists only for the JS refresh callback. A host
                    // whose dispatch has no JS handler for the synthetic event (a
                    // builtin-only plan, or a native provider definition that never
                    // installed a JS callback) never reads `context.stored`, so the
                    // store is not even read here; the restore paths below read it
                    // on demand, and the store's stamp cache makes a repeat read a
                    // metadata probe rather than a file parse.
                    let js_reads_stored = plugins.has_handlers("provider:refreshModels");
                    let mut payload = serde_json::json!({ "providerId": id });
                    if js_reads_stored {
                        if let Some(entry) = read_provider_catalog(self, id) {
                            // The entry is owned here, so move its model list into
                            // the payload instead of deep-copying it.
                            payload["stored"] = entry.into_wire();
                        }
                    }
                    // The wire list a successful refresh produced. Cached on the
                    // stamp so a later session inside the window restores the
                    // refreshed models instead of the configured (stale) ones;
                    // a failed/empty refresh caches `None` (there is no fresh
                    // list to serve).
                    let mut refreshed_wire: Option<JsonValue> = None;
                    match plugins.emit("provider:refreshModels", &mut payload).await {
                        Ok(outcome) => {
                            // `publish({persist})` persistence (upstream
                            // `ModelsPublication.persist`): `null` deletes the
                            // provider entry, an object upserts it. The JS side
                            // records the last publication on the event outcome;
                            // writing it here keeps ModelsStore the single owner
                            // of the file lock.
                            let mut persisted_in_refresh = false;
                            if let Some(publication) = outcome.result.get("persist") {
                                persisted_in_refresh = true;
                                persist_provider_catalog(self, id, publication);
                            }
                            let refreshed_config = serde_json::json!({
                                "baseUrl": config.get("baseUrl").cloned().unwrap_or(JsonValue::Null),
                                "api": config.get("api").cloned().unwrap_or(JsonValue::Null),
                                "models": outcome
                                    .result
                                    .get("models")
                                    .cloned()
                                    .unwrap_or(JsonValue::Null),
                            });
                            match parse_provider_models(&refreshed_config) {
                                Ok(refreshed) if !refreshed.is_empty() => {
                                    refreshed_wire = outcome.result.get("models").cloned();
                                    models = refreshed;
                                }
                                _ => {
                                    // Keep the persisted catalog when the refresh
                                    // returned nothing usable (the callback may be
                                    // offline or have failed). A publish that
                                    // already provided models is authoritative, so
                                    // it does not fall back to the pre-refresh
                                    // stored snapshot.
                                    let restored = (!persisted_in_refresh)
                                        .then(|| {
                                            read_provider_catalog(self, id).and_then(|entry| {
                                                restore_models_from_store(&config, id, &entry)
                                            })
                                        })
                                        .flatten();
                                    if let Some(restored) = restored {
                                        models = restored;
                                    } else {
                                        crate::ext_diag!(
                                        "[ext] provider `{id}`: refreshModels returned no usable models; keeping the configured list"
                                    );
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            // Dispatch failure is not persisted away: restore the
                            // cached catalog when one exists.
                            let restored = read_provider_catalog(self, id)
                                .and_then(|entry| restore_models_from_store(&config, id, &entry));
                            let restored_from_store = restored.is_some();
                            if let Some(restored) = restored {
                                models = restored;
                            }
                            crate::ext_diag!(
                                "[ext] provider `{id}`: refreshModels dispatch failed: {error}{}",
                                if restored_from_store {
                                    "; restored the persisted catalog"
                                } else {
                                    "; keeping the configured list"
                                }
                            );
                        }
                    }
                    mark_refresh_models(stamp_scope, id, fingerprint, refreshed_wire);
                }
            }
            // Re-apply the configured provider/model headers and authHeader to
            // whichever list won (configured, refreshed, or restored).
            apply_configured_headers(
                &mut models,
                provider_headers.as_ref(),
                auth_header,
                resolved_key.as_deref(),
            );
            // `streamSimple: true` marker (upstream `ProviderConfig.streamSimple`):
            // the provider streams through the extension's JS callback via the
            // reserved `__pir_stream:` hook channel (providers/js_stream.rs).
            if config.get("streamSimple").and_then(JsonValue::as_bool) == Some(true) {
                let Some(js) = plugins.js_handle().cloned() else {
                    crate::ext_diag!(
                        "[ext] provider `{id}` skipped: streamSimple requires the JS extension runtime"
                    );
                    continue;
                };
                match crate::ai::providers::RegisterJsStreamProviderShared::register_js_stream_provider_shared(
                    self.registry.as_ref(),
                    id.to_string(),
                    models,
                    js,
                ) {
                    Ok(_) => {}
                    Err(error) => {
                        crate::ext_diag!("[ext] provider `{id}` registration failed: {error}")
                    }
                }
                continue;
            }
            if models.is_empty() {
                // Upstream `applyExtension` (provider-composer.ts:341-351): a
                // model-less definition with a `baseUrl` OVERRIDES the URL for
                // the provider's existing models (e.g. rewriting the anthropic
                // baseUrl); the dynamic overlay wins over the static registry
                // entry on every lookup, and unregistering restores the
                // built-in. Without a `baseUrl` the definition carries no
                // applicable change.
                let Some(base_url) = config
                    .get("baseUrl")
                    .and_then(JsonValue::as_str)
                    .filter(|url| !url.is_empty())
                else {
                    continue;
                };
                let existing = self.registry.get_provider_models(id);
                if existing.is_empty() {
                    crate::ext_diag!(
                        "[ext] provider `{id}` skipped: baseUrl-only override but the provider has no known models"
                    );
                    continue;
                }
                let overridden: Vec<crate::ai::types::Model> = existing
                    .into_iter()
                    .map(|mut model| {
                        model.base_url = base_url.to_string();
                        model
                    })
                    .collect();
                let dynamic = DynamicProviderConfig {
                    base_url: base_url.to_string(),
                    models: overridden,
                    ..DynamicProviderConfig::default()
                };
                match self
                    .registry
                    .register_dynamic_provider_shared(id.to_string(), dynamic)
                {
                    Ok(_) => {}
                    Err(error) => {
                        crate::ext_diag!("[ext] provider `{id}` baseUrl override failed: {error}")
                    }
                }
                continue;
            }
            // The wire API id routes through `build_provider_from_models`
            // (providers/construction.rs), which maps `google-generative-ai`
            // to the dedicated Google adapter — the same adapter the built-in
            // catalog provider uses (upstream routes by model.api).
            let api = config
                .get("api")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            let dynamic = DynamicProviderConfig {
                base_url: config
                    .get("baseUrl")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
                api_key: config
                    .get("apiKey")
                    .and_then(JsonValue::as_str)
                    .map(interpolate_api_key),
                api,
                models,
                compat: None,
                refresh: None,
            };
            match self
                .registry
                .register_dynamic_provider_shared(id.to_string(), dynamic)
            {
                Ok(_) => {
                    if let Some(api_key) = config
                        .get("apiKey")
                        .and_then(JsonValue::as_str)
                        .map(interpolate_api_key)
                        .filter(|key| !key.is_empty())
                    {
                        self.extension_api_keys
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(id.to_string(), api_key);
                    }
                    // Upstream ProviderConfig.oauth (#12): attach the
                    // extension's OAuth flow so `/login` and the auth-status
                    // surfaces discover it through `Provider::oauth_flow` (the
                    // flow executes the JS login/refresh callbacks; the /login
                    // TUI seam drives it via the standard flow interface).
                    if config
                        .get("oauth")
                        .and_then(|oauth| oauth.get("login"))
                        .and_then(JsonValue::as_bool)
                        == Some(true)
                    {
                        if let Some(js) = plugins.js_handle().cloned() {
                            let display_name = config
                                .get("oauth")
                                .and_then(|oauth| oauth.get("name"))
                                .and_then(JsonValue::as_str)
                                .unwrap_or(id)
                                .to_string();
                            let is_subscription = config
                                .get("oauth")
                                .and_then(|oauth| oauth.get("isSubscription"))
                                .and_then(JsonValue::as_bool)
                                .unwrap_or(false);
                            let flow: crate::ai::auth::OAuthFlowBox = std::sync::Arc::new(
                                crate::ai::providers::js_stream::JsOAuthFlow::new(
                                    id.to_string(),
                                    display_name,
                                    is_subscription,
                                    js,
                                ),
                            );
                            let current = self
                                .registry
                                .dynamic
                                .write()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .get(id)
                                .cloned();
                            if let Some(inner) = current {
                                let wrapped =
                                    crate::ai::providers::js_stream::with_oauth_flow(inner, flow);
                                self.registry
                                    .dynamic
                                    .write()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .insert(id.to_string(), wrapped);
                            }
                        }
                    }
                }
                Err(error) => {
                    crate::ext_diag!("[ext] provider `{id}` registration failed: {error}")
                }
            }
        }
    }

    /// Apply `pi.registerVirtualModel` definitions (unified plugin registry:
    /// JS extensions and Rust plugins alike) to the provider registry's
    /// virtual-model overlay. Idempotent: the plugin-sourced slice is
    /// re-synced each prompt, and entries whose plugin went away are dropped.
    /// New registrations persist as `pi.virtual-model-state` custom entries so
    /// they survive resume (upstream persists router state on the branch).
    pub(super) async fn sync_plugin_virtual_models(&self, plugins: &PluginHost) {
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
        let defs = plugins.registry().virtual_model_defs();
        let desired: std::collections::BTreeSet<(String, String)> = defs
            .iter()
            .map(|def| {
                (
                    provider_of(def),
                    def.get("id")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
            .collect();
        for stale in self.registry.virtual_model_registry().all() {
            if stale.source.starts_with("plugin:")
                && !desired.contains(&(stale.provider.clone(), stale.id.clone()))
            {
                self.registry
                    .unregister_virtual_model(&stale.provider, &stale.id);
            }
        }
        for def in &defs {
            let source = def
                .get("source")
                .and_then(JsonValue::as_str)
                .unwrap_or("plugin");
            let registration =
                match crate::ai::virtual_::VirtualModelRegistration::from_wire(def, source) {
                    Ok(registration) => registration,
                    Err(error) => {
                        crate::ext_diag!("[ext] virtual model definition skipped: {error}");
                        continue;
                    }
                };
            let known = self
                .registry
                .virtual_model_registry()
                .is_registered(&registration.provider, &registration.id);
            if let Err(error) = self.registry.register_virtual_model(registration.clone()) {
                crate::ext_diag!(
                    "[ext] virtual model `{}` registration failed: {error}",
                    registration.key()
                );
                continue;
            }
            // Install/drop the per-request route callback (`route: true`
            // marker set by the JS prelude; upstream
            // `VirtualModelDefinition.route`). The callback round-trips to the
            // JS hook through the reserved `__pir_route:` channel
            // (providers/js_stream.rs); failures fall back to the static
            // route at request time.
            let route_callback = if def.get("route").and_then(JsonValue::as_bool) == Some(true) {
                plugins.js_handle().cloned().map(|js| {
                    crate::ai::providers::js_virtual_route_callback(
                        js,
                        registration.provider.clone(),
                        registration.id.clone(),
                    )
                })
            } else {
                None
            };
            self.registry.virtual_model_registry().set_route_callback(
                &registration.provider,
                &registration.id,
                route_callback,
            );
            if !known {
                self.persist_virtual_model_state(&registration).await;
            }
        }
    }

    /// Persist a virtual-model registration as a `pi.virtual-model-state`
    /// custom entry (data `{provider, modelId, state: definition}`), skipping
    /// an identical entry already on the branch.
    pub(super) async fn persist_virtual_model_state(
        &self,
        registration: &crate::ai::virtual_::VirtualModelRegistration,
    ) {
        let Some(session) = &self.session else {
            return;
        };
        let Ok(definition) = serde_json::to_value(registration) else {
            return;
        };
        let mut session = session.lock().await;
        // Borrowed lookup: `entries()` clones every entry (the whole
        // transcript) just to compare custom data. The resident list is the
        // same sequence `entries()` covers, and a header can never be the
        // custom entry being searched for.
        let exists = session.resident_entries().iter().any(|entry| {
            matches!(
                entry.as_ref(),
                crate::session_manager::FileEntry::Custom(custom)
                    if custom.custom_type == crate::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY
                        && custom.data.as_ref().and_then(|data| data.get("state"))
                            == Some(&definition)
            )
        });
        if exists {
            return;
        }
        let data = serde_json::json!({
            "provider": registration.provider,
            "modelId": registration.id,
            "state": definition,
        });
        // Upstream appends the route/registration state entry and emits
        // `entry_appended` for the persisted entry (agent-session.ts:830-834).
        let appended = match session
            .append_custom_entry(crate::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY, Some(data))
        {
            Ok(entry_id) => session
                .entry(&entry_id)
                .and_then(|entry| serde_json::to_value(entry).ok()),
            Err(error) => {
                crate::ext_diag!("[ext] failed to persist virtual-model state: {error}");
                None
            }
        };
        // Never dispatch a plugin event under the storage lock.
        drop(session);
        if let Some(entry) = appended {
            crate::agent_session::notifications::emit_entry_appended_wire(
                self.plugins.as_deref(),
                &self.session_events,
                entry,
            )
            .await;
        }
    }
    /// The session's internal `prepare_request` projection (upstream
    /// `_installAgentRequestProjection`): the host hook runs first, then a
    /// virtual selection routes the request to its physical model — the
    /// physical model handles the request while messages/usage keep the
    /// VIRTUAL identity. A registration carrying a `systemPrompt` prepends it
    /// to the request's system prompt. `routed_registration` records the
    /// registration the last request routed under.
    pub(super) fn virtual_routing_prepare_request(
        &self,
        routed_registration: Arc<
            std::sync::Mutex<Option<crate::ai::virtual_::VirtualModelRegistration>>,
        >,
    ) -> Option<PrepareRequestHook> {
        let host_hook = self
            .prepare_request
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let registry = Arc::clone(&self.registry);
        Some(Arc::new(
            move |mut request: agent_loop::PrepareRequestContext| {
                let registry = Arc::clone(&registry);
                let routed_registration = Arc::clone(&routed_registration);
                let host_hook = host_hook.clone();
                Box::pin(async move {
                    // Host hook first (upstream `previousPrepareRequest`); its
                    // update must survive even when no routing happens. The
                    // context is SHARED through the hook chain (`Arc`), so a
                    // transcript-sized context is not re-cloned per request.
                    let mut updated = false;
                    if let Some(host) = host_hook {
                        if let Some(update) = host(agent_loop::PrepareRequestContext {
                            context: Arc::clone(&request.context),
                            model: request.model.clone(),
                            thinking_level: request.thinking_level,
                        })
                        .await
                        {
                            if let Some(context) = update.context {
                                request.context = context;
                            }
                            if let Some(model) = update.model {
                                request.model = Some(model);
                            }
                            if let Some(level) = update.thinking_level {
                                request.thinking_level = level;
                            }
                            updated = true;
                        }
                    }
                    // Virtual routing: only a virtual selection routes.
                    let model = request.model.clone()?;
                    if crate::ai::virtual_::is_virtual_model(&model) {
                        match route_virtual_model(&registry, &model) {
                            Ok(Some((registration, static_physical))) => {
                                // Per-request route callback (upstream
                                // `VirtualModelDefinition.route`): consulted
                                // when the registration carries one; a
                                // failure or an invalid target falls back to
                                // the static route above (fail-open to the
                                // static path, never blocks the request).
                                let physical = match registry
                                    .virtual_model_registry()
                                    .route_callback(&model.provider, &model.id)
                                {
                                    Some(callback) => {
                                        let route_request =
                                            crate::ai::virtual_::VirtualRouteRequest {
                                                model: serde_json::to_value(&model)
                                                    .unwrap_or(JsonValue::Null),
                                                thinking_level: Some(
                                                    request.thinking_level.to_string(),
                                                ),
                                                messages: serde_json::to_value(
                                                    &request.context.messages,
                                                )
                                                .unwrap_or(JsonValue::Null),
                                            };
                                        match callback(route_request).await {
                                            Ok(decision) => registry
                                                .get_physical_model(
                                                    &decision.physical.provider,
                                                    &decision.physical.id,
                                                )
                                                .unwrap_or_else(|| {
                                                    crate::ext_diag!(
                                                        "[ext] virtual model {} route callback target {}/{} is not a physical model; using the static route",
                                                        registration.key(),
                                                        decision.physical.provider,
                                                        decision.physical.id
                                                    );
                                                    static_physical.clone()
                                                }),
                                            Err(error) => {
                                                crate::ext_diag!(
                                                    "[ext] virtual model {} route callback failed; using the static route: {error}",
                                                    registration.key()
                                                );
                                                static_physical.clone()
                                            }
                                        }
                                    }
                                    None => static_physical,
                                };
                                if let Some(extra) = &registration.system_prompt {
                                    // The registration's prompt is prepended
                                    // clone-on-write: the shared context is
                                    // cloned only when the routing actually
                                    // rewrites the prompt.
                                    let mut context = Arc::clone(&request.context);
                                    let shared = Arc::make_mut(&mut context);
                                    shared.system_prompt =
                                        Some(match shared.system_prompt.take() {
                                            Some(existing) => format!("{extra}\n\n{existing}"),
                                            None => extra.clone(),
                                        });
                                    request.context = context;
                                }
                                *routed_registration
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(registration);
                                return Some(agent_loop::PrepareRequestUpdate {
                                    // Shared through the chain: a refcount bump,
                                    // not a transcript-sized copy.
                                    context: Some(Arc::clone(&request.context)),
                                    model: Some(physical),
                                    thinking_level: Some(request.thinking_level),
                                });
                            }
                            Ok(None) => {
                                crate::ext_diag!(
                                    "[ext] virtual model {}/{} is not registered; the request will fail to stream",
                                    model.provider, model.id
                                );
                            }
                            Err(error) => {
                                crate::ext_diag!("[ext] virtual model routing failed: {error}");
                            }
                        }
                    }
                    // No routing: surface a host-hook update when one applied.
                    updated.then_some(agent_loop::PrepareRequestUpdate {
                        context: Some(request.context),
                        model: request.model,
                        thinking_level: Some(request.thinking_level),
                    })
                })
            },
        ))
    }

    /// Register a virtual model (upstream `registerVirtualModel`; embedder
    /// seam). Returns `Err` for invalid definitions and physical-id
    /// conflicts.
    pub fn register_virtual_model(
        &self,
        registration: crate::ai::virtual_::VirtualModelRegistration,
    ) -> Result<(), String> {
        self.registry.register_virtual_model(registration)
    }

    /// Remove a virtual model (upstream `unregisterVirtualModel`). Returns
    /// whether one was registered.
    pub fn unregister_virtual_model(&self, provider: &str, id: &str) -> bool {
        self.registry.unregister_virtual_model(provider, id)
    }

    /// The registered virtual models.
    pub fn virtual_models(&self) -> Vec<crate::ai::virtual_::VirtualModelRegistration> {
        self.registry.virtual_models()
    }

    /// A physical (non-virtual) catalog model (upstream `getPhysicalModel`).
    pub fn get_physical_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.registry.get_physical_model(provider, id)
    }
}

// ---------------------------------------------------------------------------
// Plugin event helpers
// ---------------------------------------------------------------------------

/// Resolve a virtual selection to the physical model that handles the request
/// (upstream `ModelRuntime.resolveModel`, static-routing form): the physical
/// model must exist in the catalog and not itself be virtual. Returns
/// `Ok(None)` when the virtual model is not registered (the request then
/// fails at the provider with "must be routed").
fn route_virtual_model(
    registry: &ProviderRegistry,
    model: &Model,
) -> Result<Option<(crate::ai::virtual_::VirtualModelRegistration, Model)>, String> {
    let Some(registration) = registry
        .virtual_model_registry()
        .get(&model.provider, &model.id)
    else {
        return Ok(None);
    };
    let physical = registry
        .get_physical_model(&registration.physical.provider, &registration.physical.id)
        .ok_or_else(|| {
            format!(
                "virtual model {} routes to {}/{}, which is not a physical model",
                registration.key(),
                registration.physical.provider,
                registration.physical.id
            )
        })?;
    Ok(Some((registration, physical)))
}
/// Restore the virtual models persisted on the session branch
/// (`pi.virtual-model-state` custom entries) into the provider registry's
/// overlay so a resumed session keeps its virtual selection routable.
///
/// A session opened TAIL-FIRST (`open_paged`) holds only its newest window
/// resident; the skipped prefix is STREAMED here (one entry at a time, in file
/// order) so a registration persisted before the window still restores —
/// without materializing the transcript at construction time, which is exactly
/// what the paged open exists to avoid.
pub(super) fn restore_session_virtual_models(
    registry: &ProviderRegistry,
    session: &Option<SessionManager>,
) {
    if let Some(session) = session {
        restore_virtual_models_from(registry, session);
    }
}

/// [`restore_session_virtual_models`] for one BORROWED manager: the shared body,
/// also called by hosts that must restore a resumed session's registrations
/// BEFORE resolving the recorded selection (the SDK builder, `attach_session`,
/// and the server's lazy rebuild) — the constructor-time restore runs after
/// those hosts have already looked the model up.
///
/// Returns the `(provider, id)` keys the branch DECLARES via its
/// `pi.virtual-model-state` entries, restored or not: a host that resolves the
/// recorded `projection.model` to nothing can then tell an unresolvable VIRTUAL
/// selection (a missing extension registration — surface an error) from a stale
/// physical id (keep the default, as before).
#[cfg_attr(not(any(feature = "sdk", feature = "server")), allow(dead_code))]
pub(crate) fn restore_virtual_models_from(
    registry: &ProviderRegistry,
    session: &SessionManager,
) -> Vec<(String, String)> {
    let mut declared = Vec::new();
    if session.is_paged() {
        // `for_each_prefix_entry` skips entries that fail to parse, like the
        // full loader's `unparsed_raw` lines do.
        let _ = session.for_each_prefix_entry(|entry| {
            if let Some(key) = restore_virtual_model_entry(registry, &entry) {
                declared.push(key);
            }
        });
    }
    // Borrowed walk: `entries()` cloned every resident entry (messages/tool
    // results included). The header cannot carry a registration, so skipping
    // it leaves the same restore sequence without the clones.
    for entry in session.resident_entries() {
        if entry.is_header() {
            continue;
        }
        if let Some(key) = restore_virtual_model_entry(registry, entry.as_ref()) {
            declared.push(key);
        }
    }
    declared
}

/// Restore one entry's virtual-model registration, when it is one (the shared
/// body of the prefix stream and the resident walk). Returns the `(provider,
/// id)` key the entry declares, even when the registration itself is rejected.
fn restore_virtual_model_entry(
    registry: &ProviderRegistry,
    entry: &crate::session_manager::FileEntry,
) -> Option<(String, String)> {
    let crate::session_manager::FileEntry::Custom(custom) = entry else {
        return None;
    };
    if custom.custom_type != crate::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY {
        return None;
    }
    // The persisted shape is `{provider, modelId, state}`; fall back to the
    // definition's own fields so a hand-written or partially malformed entry
    // still names the selection it intended.
    let declared = custom.data.as_ref().and_then(|data| {
        let state = data.get("state");
        let provider = data
            .get("provider")
            .and_then(JsonValue::as_str)
            .or_else(|| state?.get("provider").and_then(JsonValue::as_str))?;
        let id = data
            .get("modelId")
            .and_then(JsonValue::as_str)
            .or_else(|| state?.get("id").and_then(JsonValue::as_str))?;
        Some((provider.to_owned(), id.to_owned()))
    });
    let definition = custom
        .data
        .as_ref()
        .and_then(|data| data.get("state"))
        .cloned()
        .unwrap_or(JsonValue::Null);
    match crate::ai::virtual_::VirtualModelRegistration::from_wire(&definition, "session") {
        Ok(registration) => {
            if let Err(error) = registry.register_virtual_model(registration) {
                crate::ext_diag!("[ext] virtual model restore failed: {error}");
            }
        }
        Err(error) => {
            crate::ext_diag!(
                "[ext] ignoring malformed {entry_type} entry: {error}",
                entry_type = crate::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY
            )
        }
    }
    declared
}
/// Parse the `models` array of a `registerProvider` config into pir `Model`s.
/// Missing optional fields fall back to pir defaults (zero cost, text input).
fn parse_provider_models(config: &serde_json::Value) -> Result<Vec<Model>, String> {
    let Some(models) = config.get("models").and_then(serde_json::Value::as_array) else {
        return Ok(Vec::new());
    };
    let base_url = config
        .get("baseUrl")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let provider_api = config.get("api").and_then(serde_json::Value::as_str);
    let mut parsed = Vec::with_capacity(models.len());
    for model in models {
        let Some(id) = model.get("id").and_then(serde_json::Value::as_str) else {
            return Err("model entry is missing `id`".to_string());
        };
        let name = model
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(id)
            .to_string();
        let api = model
            .get("api")
            .and_then(serde_json::Value::as_str)
            .or(provider_api)
            .unwrap_or("openai-completions")
            .to_string();
        let reasoning = model
            .get("reasoning")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let input: Vec<String> = model
            .get("input")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_else(|| vec!["text".to_string()]);
        let context_window = model
            .get("contextWindow")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(128_000);
        let max_tokens = model
            .get("maxTokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(4_096);
        let base_url = model
            .get("baseUrl")
            .and_then(serde_json::Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| base_url.clone());
        if base_url.is_empty() {
            return Err(format!("model `{id}` has no baseUrl"));
        }
        // Upstream `ProviderModelConfig.type` (provider-composer.ts:54-77):
        // omitted means a chat model; `"image"` / `"classifier"` entries are
        // image-generation / structured-classifier models. The kind decides
        // which catalog operations (stream / generateImages / classify) can
        // reach the model.
        let model_type = match model.get("type").and_then(serde_json::Value::as_str) {
            Some("image") => crate::ai::types::ModelKind::Image,
            Some("classifier") => crate::ai::types::ModelKind::Classifier,
            _ => crate::ai::types::ModelKind::Chat,
        };
        parsed.push(Model {
            id: id.to_string(),
            name,
            api,
            // Provider id is stamped by DynamicProviderConfig::normalized_models.
            provider: String::new(),
            base_url,
            reasoning,
            thinking_level_map: parse_thinking_level_map(model),
            input,
            cost: parse_model_cost(model),
            context_window,
            max_tokens,
            sampling_params: None,
            headers: parse_model_headers(model),
            thinking_token_budget_field: None,
            input_limits: None,
            prompt_cache: None,
            sampling_params_by_thinking_level: None,
            model_type: Some(model_type),
            compat: None,
        });
    }
    Ok(parsed)
}

/// `thinkingLevelMap` (upstream `Model.thinkingLevelMap`): `{level: wireValue}`
/// with `null` disabling a level.
fn parse_thinking_level_map(
    model: &serde_json::Value,
) -> Option<std::collections::BTreeMap<String, Option<String>>> {
    let map = model.get("thinkingLevelMap")?.as_object()?;
    let mut parsed = std::collections::BTreeMap::new();
    for (level, value) in map {
        parsed.insert(level.clone(), value.as_str().map(String::from));
    }
    Some(parsed)
}

/// Per-model `cost` (upstream `{input, output, cacheRead, cacheWrite}` plus
/// optional request-wide `tiers`).
fn parse_model_cost(model: &serde_json::Value) -> crate::ai::types::ModelCost {
    let rates = |value: Option<&serde_json::Value>| crate::ai::types::ModelCostRates {
        input: value
            .and_then(|cost| cost.get("input"))
            .and_then(JsonValue::as_f64)
            .unwrap_or(0.0),
        output: value
            .and_then(|cost| cost.get("output"))
            .and_then(JsonValue::as_f64)
            .unwrap_or(0.0),
        cache_read: value
            .and_then(|cost| cost.get("cacheRead"))
            .and_then(JsonValue::as_f64)
            .unwrap_or(0.0),
        cache_write: value
            .and_then(|cost| cost.get("cacheWrite"))
            .and_then(JsonValue::as_f64)
            .unwrap_or(0.0),
    };
    let cost = model.get("cost");
    crate::ai::types::ModelCost {
        rates: rates(cost),
        tiers: cost
            .and_then(|cost| cost.get("tiers"))
            .and_then(JsonValue::as_array)
            .map(|tiers| {
                tiers
                    .iter()
                    .map(|tier| crate::ai::types::ModelCostTier {
                        input_tokens_above: tier
                            .get("inputTokensAbove")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0),
                        rates: rates(Some(tier)),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Per-model request `headers` (upstream `Model.headers`).
fn parse_model_headers(
    model: &serde_json::Value,
) -> Option<std::collections::BTreeMap<String, String>> {
    let headers = model.get("headers")?.as_object()?;
    let mut parsed = std::collections::BTreeMap::new();
    for (name, value) in headers {
        if let Some(value) = value.as_str() {
            parsed.insert(name.clone(), value.to_owned());
        }
    }
    Some(parsed)
}

/// Merge the provider-level `headers` and `authHeader` credential into every
/// model (provider headers first, model headers win; `authHeader` writes the
/// final `Authorization`). Re-run after any catalog replacement — a refresh
/// returns bare model configs, so without this pass every refreshed model
/// would lose the configured request headers.
fn apply_configured_headers(
    models: &mut [Model],
    provider_headers: Option<&std::collections::BTreeMap<String, String>>,
    auth_header: bool,
    resolved_key: Option<&str>,
) {
    if provider_headers.is_none() && !auth_header {
        return;
    }
    for model in models {
        let mut merged = provider_headers.cloned().unwrap_or_default();
        for (name, value) in model.headers.iter().flatten() {
            merged.insert(name.clone(), value.clone());
        }
        if auth_header {
            if let Some(key) = resolved_key {
                merged.insert("Authorization".to_string(), format!("Bearer {key}"));
            }
        }
        model.headers = (!merged.is_empty()).then_some(merged);
    }
}

/// The provider's persisted catalog for this session: the registry's injected
/// store first (it is authoritative for the live process), then — when that
/// store is memory-only, as `ProviderRegistry::default()` builds it — this
/// session's agent-dir `models-store.json` so a host that never injected a
/// [`crate::ai::providers::models_store::FileModelsStore`] still restores
/// across restarts.
fn read_provider_catalog(
    session: &AgentSession,
    provider_id: &str,
) -> Option<crate::ai::providers::models_store::ModelsStoreEntry> {
    let store = session.registry.models_store();
    match store.read(provider_id) {
        Ok(Some(entry)) => return Some(entry),
        Ok(None) => {}
        Err(error) => {
            crate::ext_diag!(
                "[ext] provider `{provider_id}`: models-store read failed: {}",
                error.user_message()
            );
        }
    }
    if store.is_file_backed() {
        return None;
    }
    crate::ai::providers::models_store::read_models_store_entry(&session.agent_dir, provider_id)
}

/// Apply one `publish({persist})` decision from the JS refresh callback to the
/// registry's [`crate::ai::providers::models_store::ModelsStore`]
/// (`null` deletes, an object upserts). Persisted model objects are stamped
/// with the provider id when they omit it, so the restored catalog is
/// provider-scoped like every catalog the store holds.
///
/// A memory-only registry store (the default) is mirrored into the session's
/// agent-dir `models-store.json`; a file-backed store already is that file and
/// is not written twice.
fn persist_provider_catalog(
    session: &AgentSession,
    provider_id: &str,
    publication: &serde_json::Value,
) {
    let store = session.registry.models_store();
    if publication.is_null() {
        if let Err(error) = store.delete(provider_id) {
            crate::ext_diag!(
                "[ext] provider `{provider_id}`: models-store delete failed: {}",
                error.user_message()
            );
        }
        if !store.is_file_backed() {
            if let Err(error) = crate::ai::providers::models_store::delete_models_store_entry(
                &session.agent_dir,
                provider_id,
            ) {
                crate::ext_diag!(
                    "[ext] provider `{provider_id}`: models-store file delete failed: {error}"
                );
            }
        }
        return;
    }
    let Some(mut entry) =
        crate::ai::providers::models_store::ModelsStoreEntry::from_wire(publication)
    else {
        crate::ext_diag!(
            "[ext] provider `{provider_id}`: publish persist is not an object; ignoring"
        );
        return;
    };
    for model in &mut entry.models {
        if let Some(object) = model.as_object_mut() {
            let missing = object
                .get("provider")
                .and_then(JsonValue::as_str)
                .is_none_or(str::is_empty);
            if missing {
                object.insert(
                    "provider".to_owned(),
                    JsonValue::String(provider_id.to_owned()),
                );
            }
        }
    }
    if let Err(error) = store.write(provider_id, entry.clone()) {
        crate::ext_diag!(
            "[ext] provider `{provider_id}`: models-store write failed: {}",
            error.user_message()
        );
    }
    if !store.is_file_backed() {
        if let Err(error) = crate::ai::providers::models_store::write_models_store_entry(
            &session.agent_dir,
            provider_id,
            entry,
        ) {
            crate::ext_diag!(
                "[ext] provider `{provider_id}`: models-store file write failed: {error}"
            );
        }
    }
}

/// Decode a persisted catalog into models, falling back to the provider config
/// for every field the store entry omitted. `None` means the entry supplies no
/// usable models.
fn restore_models_from_store(
    config: &serde_json::Value,
    provider_id: &str,
    entry: &crate::ai::providers::models_store::ModelsStoreEntry,
) -> Option<Vec<Model>> {
    if entry.models.is_empty() {
        return None;
    }
    let stored_config = serde_json::json!({
        "baseUrl": config.get("baseUrl").cloned().unwrap_or(JsonValue::Null),
        "api": config.get("api").cloned().unwrap_or(JsonValue::Null),
        "models": entry.models,
    });
    match parse_provider_models(&stored_config) {
        Ok(models) if !models.is_empty() => Some(models),
        Ok(_) => None,
        Err(error) => {
            crate::ext_diag!("[ext] provider `{provider_id}`: persisted catalog ignored: {error}");
            None
        }
    }
}

/// Provider-level request `headers` (upstream `ProviderConfig.headers`):
/// `$VAR`/`${VAR}` references interpolate from the process environment (same
/// expansion as the extension API key; upstream `resolveHeadersOrThrow`).
fn apply_provider_headers(
    config: &serde_json::Value,
) -> Option<std::collections::BTreeMap<String, String>> {
    let headers = config.get("headers")?.as_object()?;
    let mut parsed = std::collections::BTreeMap::new();
    for (name, value) in headers {
        if let Some(value) = value.as_str() {
            parsed.insert(name.clone(), interpolate_api_key(value));
        }
    }
    Some(parsed)
}

/// Expand `${VAR}` / `$VAR` references in an extension-provided API key from
/// the process environment (upstream interpolates credentials).
fn interpolate_api_key(value: &str) -> String {
    if !value.contains('$') {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len());
    let chars: Vec<char> = value.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if ch != '$' {
            out.push(ch);
            index += 1;
            continue;
        }
        // ${VAR} form first.
        if index + 1 < chars.len() && chars[index + 1] == '{' {
            if let Some(close) = chars[index + 2..].iter().position(|c| *c == '}') {
                let name: String = chars[index + 2..index + 2 + close].iter().collect();
                if !name.is_empty() {
                    if let Ok(value) = std::env::var(&name) {
                        out.push_str(&value);
                    }
                    index += close + 3;
                    continue;
                }
            }
        }
        // $VAR form.
        let mut name = String::new();
        let mut cursor = index + 1;
        while cursor < chars.len() && (chars[cursor].is_alphanumeric() || chars[cursor] == '_') {
            name.push(chars[cursor]);
            cursor += 1;
        }
        if !name.is_empty() {
            if let Ok(value) = std::env::var(&name) {
                out.push_str(&value);
            }
            index = cursor;
            continue;
        }
        out.push('$');
        index += 1;
    }
    out
}
/// Serialize the session-only model scope for the runtime's `ctx.scopedModels`
/// (upstream `ScopedModel[]`: `{model, thinkingLevel?}` entries carrying the
/// FULL serialized model object; `thinkingLevel` omitted when unset).
pub fn serialize_scoped_models(models: &[crate::ai::providers::ScopedModel]) -> JsonValue {
    JsonValue::Array(
        models
            .iter()
            .map(|scoped| {
                let mut entry = serde_json::Map::new();
                entry.insert(
                    "model".to_owned(),
                    serde_json::to_value(&scoped.model).unwrap_or(JsonValue::Null),
                );
                if let Some(level) = &scoped.thinking_level {
                    entry.insert(
                        "thinkingLevel".to_owned(),
                        JsonValue::String(level.wire_key().to_owned()),
                    );
                }
                JsonValue::Object(entry)
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Provider-scoped request credentials
// ---------------------------------------------------------------------------

/// Capacity bound of [`MODELS_JSON_KEY_CACHE`]: the most recent agent
/// directories whose `models.json` provider keys were resolved (least recently
/// used evicted first), so a process hosting agents in many directories cannot
/// grow the cache without limit.
const MODELS_JSON_KEY_CACHE_CAPACITY: usize = 4;

/// One cached `models.json` provider-key map: the agent directory it was
/// resolved for, that file's stamp when it was read, and the resolved keys.
struct ModelsJsonKeyCache {
    agent_dir: PathBuf,
    /// Length+mtime of `<agent_dir>/models.json` at read time (`None` = the
    /// file did not exist).
    stamp: Option<crate::config::settings::FileStamp>,
    keys: std::collections::BTreeMap<String, String>,
}

fn models_json_key_cache() -> &'static Mutex<Vec<ModelsJsonKeyCache>> {
    static CACHE: std::sync::OnceLock<Mutex<Vec<ModelsJsonKeyCache>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// The configured `models.json` API key of every provider, resolved against
/// `agent_dir`.
///
/// Same composition the shared loader applies when it selects a model
/// (`load_pi_config_from`) and the SDK applies for its own agent directory:
/// [`crate::ai::providers::parse_pi_config`] for the JSONC file plus
/// [`crate::ai::providers::config_value::resolve_config_value_in`] for each
/// provider's `apiKey` template (`$ENV` / `${ENV}` / literal / cached
/// `!command`). The session resolves against ITS OWN agent directory (D1), not
/// the live process environment.
///
/// **Cost.** A hit is one `metadata` call (the file stamp) plus a map clone. A
/// miss re-reads and re-parses once; `resolve_config_value_in` keys its own
/// `!command` results on the same directory, so a command value is never
/// re-run on a hot path.
///
/// **Bound.** [`MODELS_JSON_KEY_CACHE_CAPACITY`] entries, least recently used
/// evicted first. An ABSENT file caches an empty map under its `None` stamp, so
/// the common "this agent dir has no models.json" case costs one `metadata`
/// call per directory; a file that appears later changes the stamp and is read
/// on the next request.
fn models_json_api_keys(agent_dir: &Path) -> std::collections::BTreeMap<String, String> {
    let path = agent_dir.join("models.json");
    let stamp = crate::config::settings::file_stamp(&path);
    {
        let mut cache = models_json_key_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(position) = cache
            .iter()
            .position(|entry| entry.agent_dir.as_path() == agent_dir && entry.stamp == stamp)
        {
            // Move to the back: most recently used.
            let entry = cache.remove(position);
            let keys = entry.keys.clone();
            cache.push(entry);
            return keys;
        }
    }
    let mut keys = std::collections::BTreeMap::new();
    if let Ok(contents) = std::fs::read_to_string(&path) {
        if let Ok(config) = crate::ai::providers::parse_pi_config(&contents) {
            for (provider_id, provider_config) in &config.providers {
                let key = provider_config.api_key.as_deref().and_then(|template| {
                    crate::ai::providers::config_value::resolve_config_value_in(
                        agent_dir, template, None,
                    )
                });
                if let Some(key) = key.filter(|key| !key.trim().is_empty()) {
                    keys.insert(provider_id.clone(), key);
                }
            }
        }
    }
    {
        let mut cache = models_json_key_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|entry| entry.agent_dir.as_path() != agent_dir);
        while cache.len() >= MODELS_JSON_KEY_CACHE_CAPACITY {
            cache.remove(0);
        }
        cache.push(ModelsJsonKeyCache {
            agent_dir: agent_dir.to_path_buf(),
            stamp,
            keys: keys.clone(),
        });
    }
    keys
}

impl AgentSession {
    /// The API key to send with a request that runs under `model`.
    ///
    /// Provider-scoped by construction, in precedence order:
    ///
    /// 1. the explicit key the session was built with
    ///    (`AgentSessionConfig::api_key`) — but only while it belongs to this
    ///    provider, so `/model` across providers can never send it to another
    ///    endpoint;
    /// 2. a key an extension registered for this provider
    ///    (`pi.registerProvider({apiKey})`, `extension_api_keys`: the
    ///    map's key IS the provider scope);
    /// 3. the provider's own `models.json` `apiKey`, resolved from this
    ///    session's agent directory (`models_json_api_keys`, stamp-cached).
    ///
    /// `None` means "this session holds no credential for the provider": the
    /// request then carries no key, so the provider's own resolution (ambient
    /// env vars, model headers, the registry's stored credential) applies and
    /// an unconfigured provider reports its "not configured / login required"
    /// outcome instead of receiving another provider's key and answering 401.
    ///
    /// A virtual selection routes every request to a physical model, so the
    /// credential resolved is the PHYSICAL provider's — the one that actually
    /// receives the request.
    pub(super) fn request_api_key_for(&self, model: &Model) -> Option<String> {
        let routed_provider = crate::ai::virtual_::is_virtual_model(model)
            .then(|| {
                self.registry
                    .virtual_model_registry()
                    .get(&model.provider, &model.id)
                    .map(|registration| registration.physical.provider)
            })
            .flatten();
        let provider = routed_provider
            .as_deref()
            .unwrap_or(model.provider.as_str());
        if let Some(api_key) = &self.api_key {
            if api_key.provider == provider {
                return Some(api_key.key.clone());
            }
        }
        if let Some(key) = self
            .extension_api_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
        {
            if !key.trim().is_empty() {
                return Some(key.clone());
            }
        }
        models_json_api_keys(&self.agent_dir).get(provider).cloned()
    }

    /// The session's EXPLICIT provider-scoped credential (the key it was
    /// built with), as the child-run environment
    /// ([`crate::agent::tool_execution::ToolSessionEnv::credential`]) carries
    /// it.
    ///
    /// Environment/models.json credentials are deliberately NOT included: a
    /// child built with the propagated
    /// [`crate::agent::tool_execution::ToolSessionEnv::agent_dir`] resolves
    /// those itself through the same chain this session uses.
    pub(crate) fn explicit_tool_credential(
        &self,
    ) -> Option<crate::agent::tool_execution::ToolCredential> {
        self.api_key.as_ref().map(|key| {
            crate::agent::tool_execution::ToolCredential::new(key.provider.clone(), key.key.clone())
        })
    }
}
