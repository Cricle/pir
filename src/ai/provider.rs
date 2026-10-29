//! Provider trait and provider registry.
//!
//! A provider is the concrete runtime unit: it owns id/name metadata, model
//! listing, and stream behavior. The `ProviderRegistry` resolves auth and
//! delegates each request to the provider that owns the model.
//!
//! Rust equivalent of the upstream `packages/ai/src/models.ts`.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;
use tracing::Instrument;

use crate::ai::auth::ApiKeyAuth;
use crate::ai::auth::OAuthFlowBox;
use crate::ai::auth::{
    resolve_provider_auth, AuthContext, AuthInteraction, CredentialStore, LoginOptions,
    StoredCredential,
};
use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::providers::models_store::{InMemoryModelsStore, ModelsStore, ModelsStoreEntry};
use crate::ai::types::{
    calculate_cost, AssistantMessage, AssistantMessageEvent, AuthCheck, AuthType, ClientIdentity,
    Context, Credential, DeferredHandle, ErrorReason, Model, ProviderHeaders, StopReason,
    StreamOptions,
};
use crate::plugins::services::ProviderAuth;
use crate::telemetry::{
    AttributeValue, Attributes, Counter, Histogram, HistogramOptions, InstrumentOptions,
    Propagator, Span, SpanError, SpanOptions, SpanStatus, TelemetrySpan, TraceContext,
    W3cPropagator,
};

/// A boxed provider future (object-safe async provider hooks).
pub type ProviderFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// One provider's catalog publication during a refresh (upstream
/// `ModelsPublication`).
#[derive(Clone, Default)]
pub struct ModelsPublication {
    /// Persistence decision: `None` leaves storage unchanged, `Some(None)`
    /// deletes the provider's entry, `Some(Some(entry))` writes it.
    pub persist: Option<Option<ModelsStoreEntry>>,
    /// Synchronous provider-private state update, run after a successful
    /// persistence step.
    pub update: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for ModelsPublication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelsPublication")
            .field("persist", &self.persist)
            .field("update", &self.update.is_some())
            .finish()
    }
}

/// Generation-checked publication callback handed to a provider refresh
/// (upstream `RefreshModelsContext.publish`). Resolves `false` when the refresh
/// was aborted or superseded.
pub type ModelsPublisher = Arc<dyn Fn(ModelsPublication) -> ProviderFuture<bool> + Send + Sync>;

/// Context for one provider refresh phase (upstream `RefreshModelsContext`).
pub struct RefreshModelsContext {
    /// Effective credential for the provider, when one resolved. `OAuth`
    /// credentials are already refreshed.
    pub credential: Option<StoredCredential>,
    /// Immutable provider-scoped catalog snapshot captured before this phase.
    pub stored: Option<ModelsStoreEntry>,
    /// Generation-checked persistence + state publication.
    pub publisher: ModelsPublisher,
    /// False during offline/cache-only initialization.
    pub allow_network: bool,
    /// Bypass provider freshness checks when network access is allowed.
    pub force: bool,
    /// Always present: aborted when the refresh is cancelled or superseded.
    pub signal: crate::ai::types::AbortSignal,
    /// Registered provider base URL (including configuration overrides).
    pub base_url: Option<String>,
}

impl std::fmt::Debug for RefreshModelsContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshModelsContext")
            .field("credential", &self.credential)
            .field("stored", &self.stored)
            .field("allow_network", &self.allow_network)
            .field("force", &self.force)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl RefreshModelsContext {
    /// Publish a catalog update.
    pub async fn publish(&self, publication: ModelsPublication) -> bool {
        (self.publisher)(publication).await
    }
}

/// Options for [`ProviderRegistry::refresh`] (upstream
/// `ModelsRefreshOptions`).
#[derive(Debug, Clone)]
pub struct ModelsRefreshOptions {
    /// Whether providers may reach the network. Default: true.
    pub allow_network: bool,
    /// Restrict the refresh to these provider ids (unknown/static providers
    /// are ignored). `None` refreshes every refreshable provider.
    pub providers: Option<Vec<String>>,
    /// Bypass provider freshness checks.
    pub force: bool,
    /// Cancels the refresh; also encoded in the context signals.
    pub signal: Option<crate::ai::types::AbortSignal>,
}

impl Default for ModelsRefreshOptions {
    fn default() -> Self {
        Self {
            allow_network: true,
            providers: None,
            force: false,
            signal: None,
        }
    }
}

/// Result of [`ProviderRegistry::refresh`] (upstream `ModelsRefreshResult`):
/// provider errors and cancellation never reject the call.
#[derive(Debug, Clone, Default)]
pub struct ModelsRefreshResult {
    pub aborted: bool,
    pub errors: BTreeMap<String, String>,
}

/// A runtime provider.
///
/// # Implementing a provider
///
/// The required methods are [`Self::id`], [`Self::name`],
/// [`Self::get_models`], [`Self::stream`], and [`Self::api_key_auth`];
/// [`Self::stream_shared`] and [`Self::oauth_flow`] have defaults. A minimal
/// scripted/mock provider needs no network and no credential: return one
/// [`Model`] whose `provider` matches [`Self::id`], answer each request from
/// [`Self::stream`] by pushing [`AssistantMessageEvent`]s and terminating
/// with
/// [`AssistantMessageEventStream::done_event`](crate::ai::event_stream::AssistantMessageEventStream::done_event),
/// and return an empty [`Self::api_key_auth`] list when no credential is
/// needed. `docs/sdk.md` has a complete scripted-mock quickstart; the same
/// idiom is exercised by `tests/sdk_consumer_embed.rs` and
/// `tests/sdk_extensibility.rs`.
pub trait Provider: Send + Sync {
    /// Unique provider id.
    fn id(&self) -> &str;

    /// Display name.
    fn name(&self) -> &str;

    /// Current known models. Must not throw; an ill-behaved provider yields no
    /// models.
    fn get_models(&self) -> Vec<Model>;

    /// Stream an assistant message from the model.
    ///
    /// Request/model/runtime failures must be encoded in the returned stream,
    /// not thrown: error termination produces an `AssistantMessage` with
    /// `stop_reason` `Error` or `Aborted` plus an `error_message`, emitted via
    /// the stream protocol.
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream;

    /// Stream an assistant message from a SHARED transcript handle.
    ///
    /// This is the loop's provider boundary: a run keeps its transcript behind
    /// one `Arc` for its whole lifetime, so a provider that only reads the
    /// context (every adapter does) streams straight from the shared
    /// allocation instead of materializing a transcript-sized owned copy.
    ///
    /// Adapters that own their streaming entry point override this (their
    /// [`Self::stream`] delegates here with a fresh `Arc`). The default exists
    /// for providers that only implement [`Self::stream`]: it materializes the
    /// owned context once — zero-copy when this is the last handle — and
    /// delegates to the owned method.
    fn stream_shared(
        &self,
        model: &Model,
        context: Arc<Context>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let context = Arc::try_unwrap(context).unwrap_or_else(|shared| (*shared).clone());
        self.stream(model, context, options)
    }

    /// Simple stream entry point (upstream `ProviderStreams.streamSimple`).
    ///
    /// Applies the model-derived simple-request defaults
    /// ([`simple_stream_options`]: the requested thinking level is clamped to
    /// the levels the model supports) and delegates to
    /// [`Self::stream_shared`]. Adapters whose upstream `streamSimple` maps
    /// more (thinking budgets into `max_tokens`, provider-specific effort
    /// defaults) override this.
    fn stream_simple(
        &self,
        model: &Model,
        context: Arc<Context>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let options = simple_stream_options_for_context(model, &context, options);
        self.stream_shared(model, context, options)
    }

    /// Return the api-key auth methods supported by this provider, in the
    /// order [`ProviderRegistry::get_auth`] / [`ProviderRegistry::stream`]
    /// should try them.
    ///
    /// An empty vector is valid for a keyless provider — a local server or a
    /// mock driven through the SDK's `with_registry` path. That path dispatches
    /// with [`ProviderRegistry::stream_unchecked`], which needs no credential
    /// and reads no environment variables. An [`ApiKeyAuth`] with an empty
    /// env-var list also performs no environment lookup, but it resolves to
    /// `Ok(None)`; [`ProviderRegistry::stream`] reports that as "Provider is
    /// not configured", whereas `stream_unchecked` streams without auth.
    fn api_key_auth(&self) -> Vec<ApiKeyAuth>;

    /// The provider's OAuth login flow, when it has one (upstream
    /// `ProviderAuth.oauth`). Absent for api-key-only and ambient providers.
    fn oauth_flow(&self) -> Option<OAuthFlowBox> {
        None
    }

    /// Current known models of every type (upstream `Provider.getAllModels`).
    /// Providers with only chat models keep the default.
    fn get_all_models(&self) -> Vec<Model> {
        self.get_models()
    }

    /// Provider policy for credential-specific model availability (upstream
    /// `Provider.filterModels`). Applied by
    /// [`ProviderRegistry::get_available`] after auth is confirmed.
    fn filter_models(
        &self,
        models: Vec<Model>,
        _credential: Option<&StoredCredential>,
    ) -> Vec<Model> {
        models
    }

    /// Credential-specific availability policy across every model type
    /// (upstream `Provider.filterAllModels`). Defaults to
    /// [`Self::filter_models`].
    fn filter_all_models(
        &self,
        models: Vec<Model>,
        credential: Option<&StoredCredential>,
    ) -> Vec<Model> {
        self.filter_models(models, credential)
    }

    /// Provider-level static request headers (upstream `Provider.headers`).
    fn headers(&self) -> Option<ProviderHeaders> {
        None
    }

    /// Whether this provider supports [`Self::refresh_models`].
    fn supports_refresh(&self) -> bool {
        false
    }

    /// Restore and/or fetch this provider's model list (upstream
    /// `Provider.refreshModels`). `None` = no refresh implementation.
    ///
    /// A refresh runs in two phases: restore the cached catalog with
    /// `allow_network = false`, then (when allowed) fetch a newer list. A
    /// failure must retain the previous list; state changes go through
    /// `context.publish()`.
    fn refresh_models(
        &self,
        _context: RefreshModelsContext,
    ) -> Option<ProviderFuture<AiResult<()>>> {
        None
    }

    /// Fetch a deferred response and stream it (upstream
    /// `Provider.fetchDeferred`). `None` = the provider does not support
    /// deferred responses.
    fn fetch_deferred(
        &self,
        _model: &Model,
        _handle: DeferredHandle,
        _options: StreamOptions,
    ) -> Option<AssistantMessageEventStream> {
        None
    }

    /// Cancel a deferred response (upstream `Provider.cancelDeferred`). `None`
    /// = unsupported.
    fn cancel_deferred(
        &self,
        _model: &Model,
        _handle: DeferredHandle,
        _options: StreamOptions,
    ) -> Option<ProviderFuture<AiResult<()>>> {
        None
    }
}

/// Convenience alias for a boxed provider.
pub type ProviderBox = Arc<dyn Provider>;

/// Runtime collection of providers plus auth application and stream
/// convenience.
///
/// Rust equivalent of the upstream `Models` interface. This slice covers the
/// synchronous model listing and the stream path; deferred responses and
/// OAuth login flows are added in later slices.
pub struct ProviderRegistry {
    providers: BTreeMap<String, ProviderBox>,
    /// Process-unique identity for per-registry caches (provider refresh
    /// stamps). Unlike an `Arc` address, this cannot be reused while an old
    /// registry is still referenced by a cache entry.
    scope_id: u64,
    /// Runtime-registered (extension) providers, held behind an interior lock
    /// so a shared `Arc<ProviderRegistry>` can gain providers at runtime
    /// (upstream `registerProvider`). Consulted before `providers` on every
    /// lookup; see
    /// [`crate::ai::providers::RegisterDynamicProviderShared`].
    pub(crate) dynamic: std::sync::RwLock<BTreeMap<String, ProviderBox>>,
    /// Virtual-model overlay (upstream `ModelRuntime.virtualModels`):
    /// extension-registered catalog entries that route every request to a
    /// physical model. Additive to `providers`/`dynamic`; see
    /// [`crate::ai::virtual`].
    pub(crate) virtual_models: crate::ai::virtual_::VirtualModelRegistry,
    credentials: Arc<dyn CredentialStore>,
    auth_context: Arc<dyn AuthContext>,
    /// The app-owned `auth.json` store. When attached, request-time auth reads
    /// the FULL stored credential (OAuth included) through this file instead of
    /// the api-key-only [`CredentialStore`] view, mirroring upstream where the
    /// model runtime reads the app's credential store.
    auth_storage: Option<Arc<crate::ai::auth::FileAuthStorage>>,
    /// Persisted catalogs for dynamic provider refreshes (upstream
    /// `ModelsStore`). Defaults to an in-memory store.
    models_store: Arc<dyn ModelsStore>,
    /// Registry-wide default client identity; a request's
    /// [`StreamOptions::identity`] overrides it field by field.
    identity: Option<ClientIdentity>,
    /// Per-provider refresh generations and cancellation controllers
    /// (upstream `refreshGenerations` / `refreshControllers`).
    refresh_state: Arc<std::sync::Mutex<BTreeMap<String, RefreshState>>>,
}

/// One provider's in-flight refresh bookkeeping.
#[derive(Clone)]
struct RefreshState {
    generation: u64,
    controller: crate::ai::types::AbortSignal,
}

impl ProviderRegistry {
    /// Create an empty registry.
    pub fn new(credentials: Arc<dyn CredentialStore>, auth_context: Arc<dyn AuthContext>) -> Self {
        static NEXT_SCOPE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            providers: BTreeMap::new(),
            scope_id: NEXT_SCOPE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            dynamic: std::sync::RwLock::new(BTreeMap::new()),
            virtual_models: crate::ai::virtual_::VirtualModelRegistry::new(),
            credentials,
            auth_context,
            auth_storage: None,
            models_store: Arc::new(InMemoryModelsStore::new()),
            identity: None,
            refresh_state: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        }
    }

    /// Attach the catalog store used by [`Self::refresh`] (upstream
    /// `CreateModelsOptions.modelsStore`). The default is in-memory.
    pub fn with_models_store(mut self, store: Arc<dyn ModelsStore>) -> Self {
        self.models_store = store;
        self
    }

    /// Set the registry-wide client identity (upstream
    /// `CreateModelsOptions.identity`).
    pub fn with_identity(mut self, identity: ClientIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// The attached catalog store.
    pub fn models_store(&self) -> &Arc<dyn ModelsStore> {
        &self.models_store
    }

    /// Process-unique identity used as the key scope for refresh stamps.
    pub(crate) fn refresh_scope_id(&self) -> u64 {
        self.scope_id
    }

    /// The registry-wide default client identity.
    pub fn identity(&self) -> Option<&ClientIdentity> {
        self.identity.as_ref()
    }

    /// Attach the file-backed `auth.json` credential store used by
    /// request-time auth resolution (upstream: the app-owned
    /// `CredentialStore` the model runtime reads). Without it the registry
    /// only sees the injected [`CredentialStore`] — the default is in-memory,
    /// so a `/login` that wrote `auth.json` would never reach a request.
    pub fn with_auth_storage(mut self, storage: Arc<crate::ai::auth::FileAuthStorage>) -> Self {
        self.auth_storage = Some(storage);
        self
    }

    /// The attached `auth.json` storage, when any.
    pub fn auth_storage(&self) -> Option<&Arc<crate::ai::auth::FileAuthStorage>> {
        self.auth_storage.as_ref()
    }

    /// Look up a provider by id: dynamic (extension-registered) entries win
    /// over statically registered ones.
    fn provider_box(&self, id: &str) -> Option<ProviderBox> {
        if let Some(provider) = self
            .dynamic
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
        {
            return Some(provider.clone());
        }
        self.providers.get(id).cloned()
    }

    /// Register a provider (upsert by provider id). An in-flight refresh for
    /// the same id is superseded (upstream `setProvider`).
    pub fn set_provider(&mut self, provider: ProviderBox) {
        let id = provider.id().to_string();
        self.supersede_refresh(&id);
        self.providers.insert(id, provider);
    }

    /// Register a typed native provider-auth hook (login/refresh/logout) for
    /// a provider id.
    ///
    /// This is the non-plugin registration path: a provider implementation
    /// assembled outside a plugin context can install the same seam the live
    /// `ProviderService` exposes. Registration is process-wide and last-wins;
    /// [`crate::ai::auth::login_and_store`] /
    /// [`crate::ai::auth::refresh_stored_oauth_credential`] consult it before
    /// the builtin or JS flow.
    pub fn register_auth_hooks(
        &self,
        provider_id: &str,
        hooks: Arc<dyn crate::plugins::services::ProviderAuthHooks>,
    ) {
        crate::plugins::services::provider_auth_hook_registry().register(provider_id, hooks);
    }

    /// Remove the native auth hook for a provider id.
    pub fn unregister_auth_hooks(&self, provider_id: &str) -> bool {
        crate::plugins::services::provider_auth_hook_registry().unregister(provider_id)
    }

    /// The native auth hook registered for a provider id, when any.
    pub fn auth_hooks(
        &self,
        provider_id: &str,
    ) -> Option<Arc<dyn crate::plugins::services::ProviderAuthHooks>> {
        crate::plugins::services::provider_auth_hook_registry().get(provider_id)
    }

    /// All registered providers (dynamic/extension providers first).
    pub fn providers(&self) -> Vec<ProviderBox> {
        let dynamic = self
            .dynamic
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dynamic_ids: std::collections::HashSet<String> = dynamic
            .values()
            .map(|provider| provider.id().to_owned())
            .collect();
        let mut providers = dynamic.values().cloned().collect::<Vec<_>>();
        // A dynamic/extension registration overrides the static catalog entry
        // for the same id; returning both would duplicate every model in
        // `get_models`/`get_available` and let the stale static entry win some
        // lookups.
        providers.extend(
            self.providers
                .iter()
                .filter(|(id, _)| !dynamic_ids.contains(*id))
                .map(|(_, provider)| provider.clone()),
        );
        providers
    }

    /// All models from all providers (best-effort). Virtual overlay entries
    /// are appended per provider (upstream `withVirtualModels`).
    pub fn get_models(&self) -> Vec<Model> {
        let mut models = Vec::new();
        for provider in self.providers().iter() {
            models.extend(provider.get_models());
        }
        for registration in self.virtual_models.all() {
            if !models
                .iter()
                .any(|model| model.provider == registration.provider && model.id == registration.id)
            {
                let physical = self
                    .get_physical_model(&registration.physical.provider, &registration.physical.id);
                models.push(registration.catalog_model(physical.as_ref()));
            }
        }
        models
    }

    /// Models from a single provider.
    pub fn get_provider_models(&self, provider_id: &str) -> Vec<Model> {
        match self.provider_box(provider_id) {
            Some(provider) => provider.get_models(),
            None => Vec::new(),
        }
    }

    /// All models of every type from every provider, or from one provider
    /// (upstream `Models.getAllModels`, sync + best-effort). Virtual overlay
    /// entries are appended like [`Self::get_models`].
    pub fn get_all_models(&self, provider_id: Option<&str>) -> Vec<Model> {
        if let Some(provider_id) = provider_id {
            return self
                .provider_box(provider_id)
                .map(|provider| provider.get_all_models())
                .unwrap_or_default();
        }
        let mut models = Vec::new();
        for provider in self.providers().iter() {
            models.extend(provider.get_all_models());
        }
        for registration in self.virtual_models.all() {
            if !models
                .iter()
                .any(|model| model.provider == registration.provider && model.id == registration.id)
            {
                let physical = self
                    .get_physical_model(&registration.physical.provider, &registration.physical.id);
                models.push(registration.catalog_model(physical.as_ref()));
            }
        }
        models
    }

    /// Runtime model lookup against last-known lists.
    pub fn get_model(&self, provider_id: &str, model_id: &str) -> Option<Model> {
        // Virtual overlay first: a virtual model hides a physical chat model
        // with the same id (upstream `withVirtualModels`).
        if let Some(registration) = self.virtual_models.get(provider_id, model_id) {
            let physical =
                self.get_physical_model(&registration.physical.provider, &registration.physical.id);
            return Some(registration.catalog_model(physical.as_ref()));
        }
        self.get_provider_models(provider_id)
            .into_iter()
            .find(|m| m.id == model_id)
    }

    /// Register a virtual model (upsert by `(provider, id)`; upstream
    /// `registerVirtualModel`). Returns `Err` when the definition is invalid
    /// or its id belongs to a physical model of that provider.
    pub fn register_virtual_model(
        &self,
        registration: crate::ai::virtual_::VirtualModelRegistration,
    ) -> Result<(), String> {
        let existing = self
            .get_provider_models(&registration.provider)
            .into_iter()
            .find(|model| model.id == registration.id);
        if existing.is_some_and(|model| !crate::ai::virtual_::is_virtual_model(&model)) {
            return Err(format!(
                "virtual model {}/{id} conflicts with a physical model.",
                registration.provider,
                id = registration.id
            ));
        }
        self.virtual_models.register(registration)
    }

    /// Remove a virtual model (upstream `unregisterVirtualModel`). Returns
    /// whether one was registered.
    pub fn unregister_virtual_model(&self, provider: &str, id: &str) -> bool {
        self.virtual_models.unregister(provider, id)
    }

    /// Every registered virtual model, in registration-key order.
    pub fn virtual_models(&self) -> Vec<crate::ai::virtual_::VirtualModelRegistration> {
        self.virtual_models.all()
    }

    /// The virtual-model overlay registry.
    pub fn virtual_model_registry(&self) -> &crate::ai::virtual_::VirtualModelRegistry {
        &self.virtual_models
    }

    /// A catalog model that is not virtual (upstream `getPhysicalModel`).
    /// Virtual overlay entries never come back.
    pub fn get_physical_model(&self, provider_id: &str, model_id: &str) -> Option<Model> {
        self.get_provider_models(provider_id)
            .into_iter()
            .find(|m| m.id == model_id)
            .filter(|model| !crate::ai::virtual_::is_virtual_model(model))
    }

    /// Resolve provider-scoped auth for a model request.
    ///
    /// Precedence (upstream `auth/resolve.ts`): a credential stored in
    /// `auth.json` owns the provider. A stored OAuth credential resolves
    /// through the provider's OAuth flow, refreshing inside the store lock when
    /// it expires soon; a stored api-key credential resolves through the
    /// provider's api-key auth; a stored credential without a matching handler
    /// is unconfigured and does NOT fall back to ambient environment values.
    /// Only a missing credential falls back to ambient sources.
    ///
    /// Resolves to `Err` when the provider is unknown, or to `Ok(None)` when
    /// the provider is unconfigured.
    pub async fn get_auth(&self, model: &Model) -> AiResult<Option<crate::ai::types::AuthResult>> {
        self.resolve_auth(model, None).await
    }

    /// [`Self::get_auth`] with the request's explicit api-key override
    /// (upstream `AuthResolutionOverrides.apiKey`): an override resolves
    /// through the provider's api-key auth and BYPASSES the stored credential,
    /// so a `--api-key`/models.json key keeps working even when `auth.json`
    /// holds an expired OAuth entry for the same provider.
    async fn resolve_auth(
        &self,
        model: &Model,
        api_key_override: Option<&str>,
    ) -> AiResult<Option<crate::ai::types::AuthResult>> {
        let Some(provider) = self.provider_box(&model.provider) else {
            return Err(AiError::UnknownProvider(model.provider.clone()));
        };
        if let Some(key) = api_key_override.filter(|key| !key.trim().is_empty()) {
            if !provider.api_key_auth().is_empty() {
                let credential = crate::ai::types::ApiKeyCredential::new(Some(key.to_string()));
                return resolve_provider_auth(
                    provider.as_ref(),
                    Some(&crate::ai::types::Credential::ApiKey(credential)),
                    self.auth_context.as_ref(),
                )
                .await;
            }
        }
        let Some(storage) = &self.auth_storage else {
            let credential = self.credentials.read(&model.provider)?;
            return resolve_provider_auth(
                provider.as_ref(),
                credential.as_ref(),
                self.auth_context.as_ref(),
            )
            .await;
        };
        match storage.read_stored(&model.provider)? {
            Some(crate::ai::auth::StoredCredential::OAuth(credential)) => {
                // A stored credential without a matching handler stays
                // unconfigured: no silent env fallback (upstream resolve.ts).
                let Some(flow) = provider.oauth_flow() else {
                    return Ok(None);
                };
                let credential = crate::ai::auth::resolve::resolve_oauth_credential(
                    storage.clone(),
                    &model.provider,
                    flow.clone(),
                    credential,
                )
                .await?;
                let auth = flow.to_auth(&credential).map_err(|error| {
                    AiError::Authentication(format!(
                        "OAuth auth derivation failed for {}: {error}",
                        model.provider
                    ))
                })?;
                Ok(Some(crate::ai::types::AuthResult {
                    auth,
                    env: None,
                    source: Some("OAuth".to_string()),
                }))
            }
            Some(crate::ai::auth::StoredCredential::ApiKey(credential)) => {
                resolve_provider_auth(
                    provider.as_ref(),
                    Some(&crate::ai::types::Credential::ApiKey(credential)),
                    self.auth_context.as_ref(),
                )
                .await
            }
            None => {
                resolve_provider_auth(provider.as_ref(), None, self.auth_context.as_ref()).await
            }
        }
    }

    /// Stream a model request through its provider.
    ///
    /// Auth is resolved asynchronously before dispatch; auth or provider
    /// failures are encoded as an `Error` event on the returned stream so
    /// callers never see a thrown error from this method.
    pub fn stream(
        &self,
        model: Model,
        context: impl Into<Arc<Context>>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let context = context.into();
        let outer = AssistantMessageEventStream::new();
        // A producer handle (not a consumer clone): the forwarding task must
        // not keep the outer stream "consumed" on its own behalf, otherwise a
        // consumer that abandons the stream is unobservable and this task (and
        // the provider body read loop behind it) would park in `push_await`
        // forever. With a producer handle the task stops as soon as the last
        // consumer handle drops.
        let outer_events = outer.producer();
        let self_ = self.clone();
        let dispatch_model = model.clone();
        // Upstream `observeModelRequest(model, options, false)`: the request
        // span is started when the logical request is issued, before auth
        // resolution and provider setup. `None` when no telemetry span was
        // supplied, so the disabled path allocates nothing and every per-event
        // hook below is one `Option` check.
        let mut observer = ModelRequestObserver::start(&dispatch_model, &options);
        let trace_context = observer
            .as_ref()
            .map(|observer| observer.span.trace_context().clone());

        tokio::spawn(async move {
            match self_
                .resolve_and_dispatch(model, context, options, trace_context.as_ref())
                .await
            {
                Ok(inner) => {
                    let cost_model = dispatch_model.clone();
                    let mut inner = std::pin::pin!(inner);
                    // Accumulate the deltas, so the terminal message of a
                    // failed/aborted stream keeps the content the provider had
                    // already streamed (see [`StreamedContent`]).
                    let mut streamed = StreamedContent::default();
                    while let Some(event) = inner.next().await {
                        streamed.observe(&event);
                        let event = apply_model_cost(&cost_model, streamed.finalize(event));
                        if let Some(observer) = observer.as_mut() {
                            observer.observe(&event);
                        }
                        // Producer backpressure: a slow consumer parks this
                        // forwarding task (and, through the provider's body
                        // read loop, the network) instead of letting the outer
                        // queue retain frozen snapshots without bound.
                        // `false` means every consumer handle is gone: stop
                        // forwarding instead of parking forever, and terminate
                        // the inner stream so ITS producer (the transport body
                        // read loop) is released too rather than left parked on
                        // a queue nobody drains any more.
                        if !outer_events.push_await(event).await {
                            inner.as_mut().end(None);
                            break;
                        }
                    }
                    // A stream that ended without a terminal event still ends
                    // its span (upstream `observeResult(undefined)`).
                    if let Some(observer) = observer.as_mut() {
                        observer.finish();
                    }
                    outer_events.end(None);
                }
                Err(boxed) => {
                    let (message, reason) = *boxed;
                    let event = AssistantMessageEventStream::error_event(reason, message.clone());
                    // Setup failures are part of the logical request: observe
                    // the terminal error like upstream's lazyStream catch path.
                    if let Some(observer) = observer.as_mut() {
                        observer.observe(&event);
                        observer.finish();
                    }
                    outer_events.push(event);
                    outer_events.end(Some(Arc::new(message)));
                }
            }
        });

        outer
    }

    /// Stream a model request directly through the registered provider.
    ///
    /// This is intended for locally configured providers (for example Ollama)
    /// whose endpoint is intentionally keyless. Hosted requests should use
    /// [`Self::stream`], which enforces the normal auth resolver.
    pub fn stream_unchecked(
        &self,
        model: Model,
        context: impl Into<Arc<Context>>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let context = context.into();
        let outer = AssistantMessageEventStream::new();
        // Producer handle, not a consumer clone: see `stream` above — an
        // abandoned consumer must stop this task instead of parking it.
        let output = outer.producer();
        let mut observer = ModelRequestObserver::start(&model, &options);
        let trace_context = observer
            .as_ref()
            .map(|observer| observer.span.trace_context().clone());
        let Some(provider) = self.provider_box(&model.provider) else {
            let event = AssistantMessageEventStream::error_event(
                ErrorReason::Error,
                error_message(&model, &format!("Unknown provider: {}", model.provider)),
            );
            if let Some(observer) = observer.as_mut() {
                observer.observe(&event);
                observer.finish();
            }
            output.push(event);
            return outer;
        };
        let self_ = self.clone();
        tokio::spawn(async move {
            let mut options = options;
            match self_
                .assemble_dispatch_headers(provider.as_ref(), &model, &options, None)
                .await
            {
                Ok(headers) => options.headers = headers,
                Err(boxed) => {
                    let (message, reason) = *boxed;
                    let event = AssistantMessageEventStream::error_event(reason, message.clone());
                    if let Some(observer) = observer.as_mut() {
                        observer.observe(&event);
                        observer.finish();
                    }
                    output.push(event);
                    output.end(Some(Arc::new(message)));
                    return;
                }
            }
            options.headers = inject_trace_headers(options.headers, trace_context.as_ref());
            let mut inner = provider.stream_shared(&model, context, options);
            // Same terminal-message alignment as [`Self::stream`]: a failed or
            // aborted stream keeps the content it had already streamed.
            let mut streamed = StreamedContent::default();
            while let Some(event) = inner.next().await {
                streamed.observe(&event);
                let event = apply_model_cost(&model, streamed.finalize(event));
                if let Some(observer) = observer.as_mut() {
                    observer.observe(&event);
                }
                // `false`: every consumer handle is gone — drop the rest of the
                // stream rather than parking this task forever; terminating the
                // inner stream releases its producer the same way.
                if !output.push_await(event).await {
                    inner.end(None);
                    return;
                }
            }
            if let Some(observer) = observer.as_mut() {
                observer.finish();
            }
            if !output.is_done() {
                output.end(None);
            }
        });
        outer
    }

    /// Stream a request through its provider, resolving auth when one is
    /// configured but NOT requiring it.
    ///
    /// This is the agent session's request path (upstream `agent-session.ts`
    /// calls `getAuth` and streams regardless of the result): a credential in
    /// `auth.json` (OAuth or api key), a models.json `apiKey`, or ambient env
    /// auth is merged into the request; a provider with no credential at all
    /// still dispatches keyless, which is what local servers and scripted
    /// providers need. Auth resolution FAILURES (an expired OAuth credential
    /// that cannot refresh) are encoded as an `Error` event, like
    /// [`Self::stream`].
    ///
    /// Unlike [`Self::stream`], an unconfigured provider is not an error: the
    /// request goes out without auth and the endpoint decides.
    pub fn stream_resolving_auth(
        &self,
        model: Model,
        context: impl Into<Arc<Context>>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let context = context.into();
        let outer = AssistantMessageEventStream::new();
        let output = outer.producer();
        let mut observer = ModelRequestObserver::start(&model, &options);
        let trace_context = observer
            .as_ref()
            .map(|observer| observer.span.trace_context().clone());
        let Some(provider) = self.provider_box(&model.provider) else {
            let event = AssistantMessageEventStream::error_event(
                ErrorReason::Error,
                error_message(&model, &format!("Unknown provider: {}", model.provider)),
            );
            if let Some(observer) = observer.as_mut() {
                observer.observe(&event);
                observer.finish();
            }
            output.push(event);
            return outer;
        };
        let self_ = self.clone();
        tokio::spawn(async move {
            let (request_model, options) =
                match self_.resolve_auth(&model, options.api_key.as_deref()).await {
                    Ok(Some(auth)) => merge_resolved_auth(model.clone(), options, auth),
                    Ok(None) => (model.clone(), options),
                    Err(error) => {
                        let event = AssistantMessageEventStream::error_event(
                            ErrorReason::Error,
                            error_message(&model, &error.user_message()),
                        );
                        if let Some(observer) = observer.as_mut() {
                            observer.observe(&event);
                            observer.finish();
                        }
                        output.push(event);
                        return;
                    }
                };
            let mut options = options;
            match self_
                .assemble_dispatch_headers(provider.as_ref(), &request_model, &options, None)
                .await
            {
                Ok(headers) => options.headers = headers,
                Err(boxed) => {
                    let (message, reason) = *boxed;
                    let event = AssistantMessageEventStream::error_event(reason, message.clone());
                    if let Some(observer) = observer.as_mut() {
                        observer.observe(&event);
                        observer.finish();
                    }
                    output.push(event);
                    return;
                }
            }
            options.headers = inject_trace_headers(options.headers, trace_context.as_ref());
            let mut inner = provider.stream_shared(&request_model, context, options);
            // Same terminal-message alignment as [`Self::stream`]: a failed or
            // aborted stream keeps the content it had already streamed.
            let mut streamed = StreamedContent::default();
            while let Some(event) = inner.next().await {
                streamed.observe(&event);
                let event = apply_model_cost(&model, streamed.finalize(event));
                if let Some(observer) = observer.as_mut() {
                    observer.observe(&event);
                }
                if !output.push_await(event).await {
                    inner.end(None);
                    return;
                }
            }
            if let Some(observer) = observer.as_mut() {
                observer.finish();
            }
            if !output.is_done() {
                output.end(None);
            }
        });
        outer
    }
    ///
    /// Drains the event stream while waiting
    /// ([`AssistantMessageEventStream::drain_and_result`]): this consumer only
    /// wants the final message, and with a bounded queue a consumer that never
    /// pops an event would park the producer forever.
    pub async fn complete(
        &self,
        model: Model,
        context: impl Into<Arc<Context>>,
        options: StreamOptions,
    ) -> AiResult<Arc<AssistantMessage>> {
        let stream = self.stream(model, context, options);
        stream
            .drain_and_result()
            .await
            .ok_or(AiError::StreamNotTerminated)
    }

    /// Simple stream entry point (upstream `Models.streamSimple`,
    /// models.ts:929-939): applies the model-derived simple defaults
    /// ([`simple_stream_options`]) and streams through the normal auth path.
    pub fn stream_simple(
        &self,
        model: Model,
        context: impl Into<Arc<Context>>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let context = context.into();
        let options = simple_stream_options_for_context(&model, &context, options);
        self.stream(model, context, options)
    }

    /// Simple completion (upstream `Models.completeSimple`, models.ts:941-948):
    /// [`Self::stream_simple`] drained to its terminal message.
    pub async fn complete_simple(
        &self,
        model: Model,
        context: impl Into<Arc<Context>>,
        options: StreamOptions,
    ) -> AiResult<Arc<AssistantMessage>> {
        let stream = self.stream_simple(model, context, options);
        stream
            .drain_and_result()
            .await
            .ok_or(AiError::StreamNotTerminated)
    }

    async fn resolve_and_dispatch(
        &self,
        model: Model,
        context: Arc<Context>,
        options: StreamOptions,
        trace_context: Option<&TraceContext>,
    ) -> Result<AssistantMessageEventStream, Box<(AssistantMessage, ErrorReason)>> {
        // Dispatch span (upstream `TelemetryContext.startSpan` around provider
        // dispatch): carries the resolved provider/model/api identity. The span
        // covers auth resolution and header merging only — stream consumption
        // stays span-free to avoid per-event noise.
        let span = tracing::info_span!(
            "provider.dispatch",
            provider = %model.provider,
            model = %model.id,
            api = %model.api
        );
        self.resolve_and_dispatch_in_span(model, context, options, trace_context)
            .instrument(span)
            .await
    }

    /// Body of [`Self::resolve_and_dispatch`], executed inside the
    /// `provider.dispatch` span.
    async fn resolve_and_dispatch_in_span(
        &self,
        model: Model,
        context: Arc<Context>,
        options: StreamOptions,
        trace_context: Option<&TraceContext>,
    ) -> Result<AssistantMessageEventStream, Box<(AssistantMessage, ErrorReason)>> {
        let provider = self.provider_box(&model.provider).ok_or_else(|| {
            tracing::warn!(
                error = %format!("Unknown provider: {}", model.provider),
                "provider dispatch failed"
            );
            Box::new((
                error_message(&model, &format!("Unknown provider: {}", model.provider)),
                ErrorReason::Error,
            ))
        })?;

        let auth = match self.resolve_auth(&model, options.api_key.as_deref()).await {
            Ok(Some(auth)) => auth,
            Ok(None) => {
                tracing::warn!(
                    error = "provider is not configured",
                    "provider dispatch failed"
                );
                return Err(Box::new((
                    error_message(&model, "Provider is not configured"),
                    ErrorReason::Error,
                )));
            }
            Err(error) => {
                tracing::warn!(error = %error.user_message(), "provider dispatch failed");
                return Err(Box::new((
                    error_message(&model, &error.user_message()),
                    ErrorReason::Error,
                )));
            }
        };

        // Explicit request options win per-field. When the caller supplied an
        // explicit api key (models.json `apiKey`, `--api-key`, a stored key),
        // drop a resolved `Authorization` header from ambient bearer env vars
        // (`ANTHROPIC_AUTH_TOKEN`): the explicit key must be the credential
        // that wins, not a second header the provider may honor first.
        let crate::ai::types::ModelAuth {
            api_key: auth_api_key,
            headers: auth_headers,
            base_url: auth_base_url,
        } = auth.auth;
        let api_key = options.api_key.clone().or(auth_api_key);
        let auth_headers = if options.api_key.is_some() {
            auth_headers.map(|headers| {
                headers
                    .into_iter()
                    .filter(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
                    .collect()
            })
        } else {
            auth_headers
        };
        let env = options.env.clone().or(auth.env);

        let request_model = match auth_base_url {
            Some(base_url) => Model {
                base_url,
                ..model.clone()
            },
            None => model.clone(),
        };
        // Materialize the Cloudflare Workers AI / AI Gateway endpoint
        // placeholders from the resolved auth env, exactly like
        // `merge_resolved_auth` does for the non-strict streaming path: the
        // adapters send `model.base_url` verbatim, so an unresolved
        // `{CLOUDFLARE_ACCOUNT_ID}`/`{CLOUDFLARE_GATEWAY_ID}` would otherwise
        // reach the wire.
        let request_model = match env.as_ref() {
            Some(env) => {
                let base_url =
                    crate::ai::auth::resolve_cloudflare_base_url(&request_model.base_url, env);
                if base_url == request_model.base_url {
                    request_model
                } else {
                    Model {
                        base_url,
                        ..request_model
                    }
                }
            }
            None => request_model,
        };

        // Provider/model/identity layers under the resolved auth headers and
        // the caller's explicit headers, then the models-level header
        // transform and trace propagation.
        let mut options = StreamOptions {
            api_key,
            env,
            ..options
        };
        options.headers = self
            .assemble_dispatch_headers(
                provider.as_ref(),
                &request_model,
                &options,
                auth_headers.as_ref(),
            )
            .await?;
        options.headers = inject_trace_headers(options.headers, trace_context);
        Ok(provider.stream_shared(&request_model, context, options))
    }

    /// Assemble the outgoing header set for one request from every layer:
    /// identity, provider-static, model-static, session/attribution, resolved
    /// auth headers, the caller's explicit headers, then the caller's header
    /// transform. Suppression markers (empty values) are preserved so the
    /// transport can drop the header.
    async fn assemble_dispatch_headers(
        &self,
        provider: &dyn Provider,
        model: &Model,
        options: &StreamOptions,
        auth_headers: Option<&ProviderHeaders>,
    ) -> Result<Option<ProviderHeaders>, Box<(AssistantMessage, ErrorReason)>> {
        let identity = ClientIdentity::merge(self.identity.as_ref(), options.identity.as_ref());
        let mut layers = ProviderHeaders::new();
        if let Some(identity) = identity.as_ref() {
            extend_headers(
                &mut layers,
                &crate::ai::types::identity_headers(
                    &model.provider,
                    &model.base_url,
                    Some(identity),
                ),
            );
        }
        if let Some(provider_headers) = provider.headers() {
            extend_headers(&mut layers, &provider_headers);
        }
        if let Some(model_headers) = model.headers.as_ref() {
            extend_headers(&mut layers, model_headers);
        }
        if let Some(session) = session_headers(model, options.session_id.as_deref()) {
            extend_headers(&mut layers, &session);
        }
        if identity.is_none() {
            if let Some(attribution) =
                default_attribution_headers(model, install_telemetry_enabled())
            {
                extend_headers(&mut layers, &attribution);
            }
        }
        if let Some(auth_headers) = auth_headers {
            extend_headers(&mut layers, auth_headers);
        }
        if let Some(explicit) = options.headers.as_ref() {
            extend_headers(&mut layers, explicit);
        }
        let headers = if layers.is_empty() {
            None
        } else {
            Some(layers)
        };
        let mut headers = apply_auth_header(
            model,
            headers,
            options.headers.as_ref(),
            options.api_key.as_deref(),
        );
        if let Some(transform) = options.transform_headers.hook() {
            let assembled = headers.take().unwrap_or_default();
            headers = Some(transform(assembled).await.map_err(|error| {
                Box::new((
                    error_message(model, &error.user_message()),
                    ErrorReason::Error,
                ))
            })?);
        }
        Ok(headers)
    }
}

// ---------------------------------------------------------------------------
// Models management API (upstream `Models`: refresh/checkAuth/getAvailable/
// login/logout/getAllModels)
// ---------------------------------------------------------------------------

impl ProviderRegistry {
    /// The stored credential for a provider: the full `auth.json` credential
    /// when the app store is attached, else the injected [`CredentialStore`]
    /// view.
    fn stored_credential(&self, provider_id: &str) -> AiResult<Option<StoredCredential>> {
        match &self.auth_storage {
            Some(storage) => storage.read_stored(provider_id),
            None => Ok(self
                .credentials
                .read(provider_id)?
                .as_ref()
                .map(StoredCredential::from)),
        }
    }

    /// Check one provider's auth without refreshing OAuth tokens (upstream
    /// `checkProviderAuth`).
    async fn check_provider_auth(
        &self,
        provider: &dyn Provider,
        credential: Option<&StoredCredential>,
    ) -> AiResult<Option<AuthCheck>> {
        match credential {
            Some(StoredCredential::OAuth(_)) => {
                Ok(provider.oauth_flow().is_some().then(|| AuthCheck {
                    source: Some("OAuth".to_string()),
                    auth_type: AuthType::OAuth,
                }))
            }
            Some(StoredCredential::ApiKey(credential)) => {
                if provider.api_key_auth().is_empty() {
                    return Ok(None);
                }
                let credential = Credential::ApiKey(credential.clone());
                let resolved =
                    resolve_provider_auth(provider, Some(&credential), self.auth_context.as_ref())
                        .await?;
                Ok(resolved.map(|resolved| AuthCheck {
                    source: resolved.source,
                    auth_type: AuthType::ApiKey,
                }))
            }
            None => {
                if provider.api_key_auth().is_empty() {
                    return Ok(None);
                }
                let resolved =
                    resolve_provider_auth(provider, None, self.auth_context.as_ref()).await?;
                Ok(resolved.map(|resolved| AuthCheck {
                    source: resolved.source,
                    auth_type: AuthType::ApiKey,
                }))
            }
        }
    }

    /// Whether a provider has complete auth configuration, without refreshing
    /// OAuth (upstream `Models.checkAuth`). `Ok(None)` for an unknown or
    /// unconfigured provider.
    pub async fn check_auth(&self, provider_id: &str) -> AiResult<Option<AuthCheck>> {
        let Some(provider) = self.provider_box(provider_id) else {
            return Ok(None);
        };
        let credential = self.stored_credential(provider_id)?;
        self.check_provider_auth(provider.as_ref(), credential.as_ref())
            .await
    }

    /// Chat models whose providers have complete auth configuration (upstream
    /// `Models.getAvailable`). Applies each provider's
    /// [`Provider::filter_models`] policy.
    pub async fn get_available(&self, provider_id: Option<&str>) -> AiResult<Vec<Model>> {
        let providers = match provider_id {
            Some(provider_id) => self.provider_box(provider_id).into_iter().collect(),
            None => self.providers(),
        };
        let mut available = Vec::new();
        for provider in providers {
            let credential = self.stored_credential(provider.id())?;
            if self
                .check_provider_auth(provider.as_ref(), credential.as_ref())
                .await?
                .is_none()
            {
                continue;
            }
            let models = provider.get_models();
            available.extend(provider.filter_models(models, credential.as_ref()));
        }
        Ok(available)
    }

    /// Models of every type whose providers have complete auth configuration
    /// (upstream `Models.getAllAvailable`). Applies
    /// [`Provider::filter_all_models`].
    pub async fn get_all_available(&self, provider_id: Option<&str>) -> AiResult<Vec<Model>> {
        let providers = match provider_id {
            Some(provider_id) => self.provider_box(provider_id).into_iter().collect(),
            None => self.providers(),
        };
        let mut available = Vec::new();
        for provider in providers {
            let credential = self.stored_credential(provider.id())?;
            if self
                .check_provider_auth(provider.as_ref(), credential.as_ref())
                .await?
                .is_none()
            {
                continue;
            }
            let models = provider.get_all_models();
            available.extend(provider.filter_all_models(models, credential.as_ref()));
        }
        Ok(available)
    }

    /// Run a provider-owned login flow and persist the credential (upstream
    /// `Models.login`).
    ///
    /// OAuth uses the provider's [`Provider::oauth_flow`] and requires an
    /// attached [`Self::with_auth_storage`] file store. Api-key login prompts
    /// the interaction for a secret and stores it in the attached file store,
    /// or in the injected [`CredentialStore`] when no file store is attached.
    pub async fn login(
        &self,
        provider_id: &str,
        auth_type: AuthType,
        interaction: &dyn AuthInteraction,
        options: &LoginOptions,
    ) -> AiResult<ProviderAuth> {
        let provider = self
            .provider_box(provider_id)
            .ok_or_else(|| AiError::UnknownProvider(provider_id.to_string()))?;
        match auth_type {
            AuthType::OAuth => {
                let flow = provider.oauth_flow().ok_or_else(|| {
                    AiError::Authentication(format!(
                        "{} does not support OAuth login",
                        provider.name()
                    ))
                })?;
                let storage = self.auth_storage.as_ref().ok_or_else(|| {
                    AiError::Authentication(
                        "no auth storage is attached; OAuth login needs an auth.json store"
                            .to_string(),
                    )
                })?;
                crate::ai::auth::login_and_store(
                    storage.as_ref(),
                    provider_id,
                    flow.as_ref(),
                    interaction,
                    options,
                )
                .await
            }
            AuthType::ApiKey => {
                if provider.api_key_auth().is_empty() {
                    return Err(AiError::Authentication(format!(
                        "{} does not support api-key login",
                        provider.name()
                    )));
                }
                let key = interaction
                    .prompt(crate::ai::auth::AuthPrompt::Secret {
                        message: format!("Enter {} API key", provider.name()),
                        placeholder: None,
                    })
                    .await?
                    .trim()
                    .to_string();
                if key.is_empty() {
                    return Err(AiError::Authentication(
                        "no API key was entered".to_string(),
                    ));
                }
                let credential = StoredCredential::ApiKey(crate::ai::types::ApiKeyCredential::new(
                    Some(key.clone()),
                ));
                match &self.auth_storage {
                    Some(storage) => storage.store(provider_id, credential)?,
                    None => {
                        self.credentials.modify(provider_id, &mut |_| {
                            Ok(Some(Credential::ApiKey(
                                crate::ai::types::ApiKeyCredential::new(Some(key.clone())),
                            )))
                        })?;
                    }
                }
                Ok(ProviderAuth::api_key(key))
            }
        }
    }

    /// Remove the stored credential for a provider (upstream
    /// `Models.logout`). Runs the registered native logout hook first when the
    /// app file store is attached.
    pub async fn logout(&self, provider_id: &str) -> AiResult<()> {
        match &self.auth_storage {
            Some(storage) => {
                crate::ai::auth::cli::logout_stored(storage.clone(), provider_id).await?;
            }
            None => self.credentials.delete(provider_id)?,
        }
        Ok(())
    }

    /// Refresh selected configured dynamic providers concurrently (upstream
    /// `Models.refresh`). Provider errors and cancellation are returned in the
    /// result instead of rejecting; static/unknown/unconfigured providers are
    /// skipped.
    pub async fn refresh(&self, options: ModelsRefreshOptions) -> ModelsRefreshResult {
        let caller_signal = options.signal.clone().unwrap_or_default();
        if caller_signal.is_aborted() {
            return ModelsRefreshResult {
                aborted: true,
                errors: BTreeMap::new(),
            };
        }
        let selected: Option<std::collections::BTreeSet<String>> = options
            .providers
            .as_ref()
            .map(|providers| providers.iter().cloned().collect());
        let mut ids: Vec<String> = self
            .providers()
            .iter()
            .map(|provider| provider.id().to_string())
            .collect();
        ids.sort();
        ids.dedup();
        let refreshable = ids.into_iter().filter_map(|id| {
            if selected
                .as_ref()
                .is_some_and(|selected| !selected.contains(&id))
            {
                return None;
            }
            self.provider_box(&id)
                .filter(|provider| provider.supports_refresh())
        });
        let outcomes = futures_util::future::join_all(refreshable.map(|provider| {
            self.refresh_provider(
                provider,
                &caller_signal,
                options.allow_network,
                options.force,
            )
        }))
        .await;
        let mut errors = BTreeMap::new();
        for (id, outcome) in outcomes {
            if let Err(error) = outcome {
                errors.insert(id, error);
            }
        }
        ModelsRefreshResult {
            aborted: caller_signal.is_aborted(),
            errors,
        }
    }

    /// Refresh one provider through both phases; returns `(provider id,
    /// outcome)` so errors are keyed even on failure.
    async fn refresh_provider(
        &self,
        provider: ProviderBox,
        caller_signal: &crate::ai::types::AbortSignal,
        allow_network: bool,
        force: bool,
    ) -> (String, Result<(), String>) {
        let id = provider.id().to_string();
        let (generation, controller) = self.begin_refresh(&id);
        let signal = crate::ai::types::AbortSignal::any([caller_signal.clone(), controller]);
        let outcome = async {
            let stored = self
                .models_store
                .read(&id)
                .map_err(|error| error.user_message())?;
            let stored_credential = self.stored_credential(&id).ok().flatten();
            // Phase 1: restore the cached catalog before any auth/network work.
            let context = self.refresh_context(
                &id,
                &provider,
                stored_credential.clone(),
                stored.clone(),
                false,
                None,
                generation,
                &signal,
            );
            let refresh = provider
                .refresh_models(context)
                .ok_or_else(|| format!("provider {id} does not support refresh"))?;
            refresh.await.map_err(|error| error.user_message())?;
            if !allow_network || signal.is_aborted() {
                return Ok(());
            }
            // Phase 2: resolve the effective credential, then fetch.
            let Some(credential) = self
                .resolve_refresh_credential(provider.as_ref(), stored_credential, &signal)
                .await
            else {
                return Ok(());
            };
            let context = self.refresh_context(
                &id,
                &provider,
                Some(credential),
                stored,
                true,
                Some(force),
                generation,
                &signal,
            );
            let refresh = provider
                .refresh_models(context)
                .ok_or_else(|| format!("provider {id} does not support refresh"))?;
            refresh.await.map_err(|error| error.user_message())
        }
        .await;
        self.end_refresh(&id, generation);
        (id, outcome)
    }

    /// Resolve the credential used for a refresh's network phase (upstream
    /// `resolveRefreshCredential`): a stored OAuth credential is refreshed
    /// when it expired soon; an api-key credential resolves through the
    /// provider's api-key auth. `None` means "do not fetch".
    async fn resolve_refresh_credential(
        &self,
        provider: &dyn Provider,
        stored: Option<StoredCredential>,
        signal: &crate::ai::types::AbortSignal,
    ) -> Option<StoredCredential> {
        match stored {
            Some(StoredCredential::OAuth(credential)) => {
                let flow = provider.oauth_flow()?;
                let now = crate::ai::util::now_ms() as i64;
                if now < credential.expires {
                    return Some(StoredCredential::OAuth(credential));
                }
                if signal.is_aborted() {
                    return None;
                }
                let storage = self.auth_storage.clone()?;
                let refreshed = crate::ai::auth::resolve::refresh_stored_oauth_credential(
                    storage,
                    provider.id(),
                    flow,
                    move |current| now >= current.expires,
                )
                .await
                .ok()??;
                Some(StoredCredential::OAuth(refreshed))
            }
            stored => {
                if provider.api_key_auth().is_empty() {
                    return None;
                }
                let credential = stored.and_then(|stored| match stored {
                    StoredCredential::ApiKey(credential) => Some(credential),
                    StoredCredential::OAuth(_) => None,
                });
                let auth_credential = credential
                    .as_ref()
                    .map(|credential| Credential::ApiKey(credential.clone()));
                let resolved = resolve_provider_auth(
                    provider,
                    auth_credential.as_ref(),
                    self.auth_context.as_ref(),
                )
                .await
                .ok()??;
                let key = resolved.auth.api_key?;
                Some(StoredCredential::ApiKey(
                    crate::ai::types::ApiKeyCredential {
                        credential_type: crate::ai::types::CredentialType::ApiKey,
                        key: Some(key),
                        env: resolved.env,
                    },
                ))
            }
        }
    }

    /// Build the per-phase refresh context with a generation-checked publisher.
    #[allow(clippy::too_many_arguments)]
    fn refresh_context(
        &self,
        provider_id: &str,
        provider: &ProviderBox,
        credential: Option<StoredCredential>,
        stored: Option<ModelsStoreEntry>,
        allow_network: bool,
        force: Option<bool>,
        generation: u64,
        signal: &crate::ai::types::AbortSignal,
    ) -> RefreshModelsContext {
        let store = self.models_store.clone();
        let state = self.refresh_state.clone();
        let provider_id = provider_id.to_string();
        let publisher_signal = signal.clone();
        let publisher: ModelsPublisher = Arc::new(move |publication| {
            let store = store.clone();
            let state = state.clone();
            let provider_id = provider_id.clone();
            let signal = publisher_signal.clone();
            Box::pin(async move {
                if signal.is_aborted() || !refresh_generation_is(&state, &provider_id, generation) {
                    return false;
                }
                match publication.persist {
                    Some(Some(entry)) => {
                        if store.write(&provider_id, entry).is_err() {
                            return false;
                        }
                    }
                    Some(None) if store.delete(&provider_id).is_err() => {
                        return false;
                    }
                    Some(None) => {}
                    None => {}
                }
                if signal.is_aborted() || !refresh_generation_is(&state, &provider_id, generation) {
                    return false;
                }
                if let Some(update) = publication.update {
                    update();
                }
                true
            })
        });
        let base_url = provider
            .get_models()
            .first()
            .map(|model| model.base_url.clone());
        RefreshModelsContext {
            credential,
            stored,
            publisher,
            allow_network,
            force: force.unwrap_or(false),
            signal: signal.clone(),
            base_url,
        }
    }

    /// Start a refresh: bump the generation and abort the previous controller.
    fn begin_refresh(&self, provider_id: &str) -> (u64, crate::ai::types::AbortSignal) {
        let controller = crate::ai::types::AbortSignal::new();
        let mut state = self
            .refresh_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = state
            .get(provider_id)
            .map(|entry| entry.generation)
            .unwrap_or(0)
            + 1;
        if let Some(previous) = state.get(provider_id) {
            previous.controller.abort();
        }
        state.insert(
            provider_id.to_string(),
            RefreshState {
                generation,
                controller: controller.clone(),
            },
        );
        (generation, controller)
    }

    /// Finish a refresh: drop the controller when it is still the current one.
    fn end_refresh(&self, provider_id: &str, generation: u64) {
        let mut state = self
            .refresh_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .get(provider_id)
            .is_some_and(|entry| entry.generation == generation)
        {
            state.remove(provider_id);
        }
    }

    /// Invalidate any in-flight refresh for a provider id.
    fn supersede_refresh(&self, provider_id: &str) {
        let mut state = self
            .refresh_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state.get(provider_id) {
            entry.controller.abort();
        }
        let generation = state
            .get(provider_id)
            .map(|entry| entry.generation)
            .unwrap_or(0)
            + 1;
        state.insert(
            provider_id.to_string(),
            RefreshState {
                generation,
                controller: crate::ai::types::AbortSignal::new(),
            },
        );
    }

    // -----------------------------------------------------------------------
    // Deferred responses (upstream `Models.streamDeferred/fetchDeferred/
    // cancelDeferred`)
    // -----------------------------------------------------------------------

    /// Stream a deferred response through its provider (upstream
    /// `Models.streamDeferred`). Auth is resolved like [`Self::stream`];
    /// setup failures are encoded as an `Error` event.
    pub fn stream_deferred(
        &self,
        model: Model,
        handle: DeferredHandle,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let outer = AssistantMessageEventStream::new();
        let output = outer.producer();
        let mut observer = ModelRequestObserver::start(&model, &options);
        let trace_context = observer
            .as_ref()
            .map(|observer| observer.span.trace_context().clone());
        let self_ = self.clone();
        tokio::spawn(async move {
            match self_
                .resolve_deferred_dispatch(model.clone(), handle, options, trace_context.as_ref())
                .await
            {
                Ok(inner) => {
                    let cost_model = model.clone();
                    let mut inner = std::pin::pin!(inner);
                    let mut streamed = StreamedContent::default();
                    while let Some(event) = inner.next().await {
                        streamed.observe(&event);
                        let event = apply_model_cost(&cost_model, streamed.finalize(event));
                        if let Some(observer) = observer.as_mut() {
                            observer.observe(&event);
                        }
                        if !output.push_await(event).await {
                            inner.as_mut().end(None);
                            break;
                        }
                    }
                    if let Some(observer) = observer.as_mut() {
                        observer.finish();
                    }
                    output.end(None);
                }
                Err(boxed) => {
                    let (message, reason) = *boxed;
                    let event = AssistantMessageEventStream::error_event(reason, message.clone());
                    if let Some(observer) = observer.as_mut() {
                        observer.observe(&event);
                        observer.finish();
                    }
                    output.push(event);
                    output.end(Some(Arc::new(message)));
                }
            }
        });
        outer
    }

    /// Fetch a deferred response and wait for its terminal message (upstream
    /// `Models.fetchDeferred`).
    pub async fn fetch_deferred(
        &self,
        model: Model,
        handle: DeferredHandle,
        options: StreamOptions,
    ) -> AiResult<Arc<AssistantMessage>> {
        self.stream_deferred(model, handle, options)
            .drain_and_result()
            .await
            .ok_or(AiError::StreamNotTerminated)
    }

    /// Cancel a deferred response (upstream `Models.cancelDeferred`).
    pub async fn cancel_deferred(
        &self,
        model: Model,
        handle: DeferredHandle,
        options: StreamOptions,
    ) -> AiResult<()> {
        let provider = self
            .provider_box(&model.provider)
            .ok_or_else(|| AiError::UnknownProvider(model.provider.clone()))?;
        let (request_model, options) = match self
            .resolve_auth(&model, options.api_key.as_deref())
            .await?
        {
            Some(auth) => merge_resolved_auth(model.clone(), options, auth),
            None => {
                return Err(AiError::ProviderNotConfigured(model.provider.clone()));
            }
        };
        let Some(cancel) = provider.cancel_deferred(&request_model, handle, options) else {
            return Err(AiError::NonRetryable(format!(
                "Provider {} does not support deferred responses",
                model.provider
            )));
        };
        cancel.await
    }

    /// Resolve auth/headers and dispatch one deferred fetch to its provider.
    async fn resolve_deferred_dispatch(
        &self,
        model: Model,
        handle: DeferredHandle,
        options: StreamOptions,
        trace_context: Option<&TraceContext>,
    ) -> Result<AssistantMessageEventStream, Box<(AssistantMessage, ErrorReason)>> {
        let provider = self.provider_box(&model.provider).ok_or_else(|| {
            Box::new((
                error_message(&model, &format!("Unknown provider: {}", model.provider)),
                ErrorReason::Error,
            ))
        })?;
        let auth = match self.resolve_auth(&model, options.api_key.as_deref()).await {
            Ok(Some(auth)) => auth,
            Ok(None) => {
                return Err(Box::new((
                    error_message(&model, "Provider is not configured"),
                    ErrorReason::Error,
                )))
            }
            Err(error) => {
                return Err(Box::new((
                    error_message(&model, &error.user_message()),
                    ErrorReason::Error,
                )))
            }
        };
        let (request_model, mut options) = merge_resolved_auth(model, options, auth);
        options.headers = self
            .assemble_dispatch_headers(provider.as_ref(), &request_model, &options, None)
            .await?;
        options.headers = inject_trace_headers(options.headers, trace_context);
        let Some(stream) = provider.fetch_deferred(&request_model, handle, options) else {
            return Err(Box::new((
                error_message(
                    &request_model,
                    &format!(
                        "Provider {} does not support deferred responses",
                        request_model.provider
                    ),
                ),
                ErrorReason::Error,
            )));
        };
        Ok(stream)
    }
}

/// Whether `provider_id`'s refresh generation still equals `generation`.
fn refresh_generation_is(
    state: &std::sync::Mutex<BTreeMap<String, RefreshState>>,
    provider_id: &str,
    generation: u64,
) -> bool {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(provider_id)
        .is_some_and(|entry| entry.generation == generation)
}

impl Clone for ProviderRegistry {
    fn clone(&self) -> Self {
        Self {
            providers: self.providers.clone(),
            scope_id: self.scope_id,
            dynamic: std::sync::RwLock::new(
                self.dynamic
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            ),
            virtual_models: self.virtual_models.clone(),
            credentials: self.credentials.clone(),
            auth_context: self.auth_context.clone(),
            auth_storage: self.auth_storage.clone(),
            models_store: self.models_store.clone(),
            identity: self.identity.clone(),
            refresh_state: self.refresh_state.clone(),
        }
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new(
            Arc::new(crate::ai::auth::InMemoryCredentialStore::new()),
            Arc::new(crate::ai::auth::EnvAuthContext::new()),
        )
    }
}

/// Merge resolved auth into a request's model/options for
/// [`ProviderRegistry::stream_resolving_auth`], mirroring
/// [`ProviderRegistry::resolve_and_dispatch_in_span`]: the request's explicit
/// api key wins over the resolved one, an explicit key drops a resolved
/// `Authorization` header, explicit headers win per name, and a resolved
/// `base_url` (GitHub Copilot) replaces the model's. Cloudflare gateway /
/// workers endpoint placeholders are materialized from the resolved env so
/// every api adapter sees a concrete URL.
fn merge_resolved_auth(
    model: Model,
    options: StreamOptions,
    auth: crate::ai::types::AuthResult,
) -> (Model, StreamOptions) {
    let crate::ai::types::ModelAuth {
        api_key: auth_api_key,
        headers: auth_headers,
        base_url: auth_base_url,
    } = auth.auth;
    let api_key = options.api_key.clone().or(auth_api_key);
    let auth_headers = if options.api_key.is_some() {
        auth_headers.map(|headers| {
            headers
                .into_iter()
                .filter(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
                .collect()
        })
    } else {
        auth_headers
    };
    let env = options.env.clone().or(auth.env);
    let request_model = match auth_base_url {
        Some(base_url) => Model {
            base_url,
            ..model.clone()
        },
        None => model.clone(),
    };
    let request_model = match env.as_ref() {
        Some(env) => {
            let base_url =
                crate::ai::auth::resolve_cloudflare_base_url(&request_model.base_url, env);
            if base_url == request_model.base_url {
                request_model
            } else {
                Model {
                    base_url,
                    ..request_model
                }
            }
        }
        None => request_model,
    };
    let headers = explicit_headers(options.headers.as_ref(), auth_headers.as_ref());
    (
        request_model,
        StreamOptions {
            api_key,
            env,
            headers,
            ..options
        },
    )
}

fn extend_headers(base: &mut ProviderHeaders, overlay: &ProviderHeaders) {
    for (name, value) in overlay {
        crate::ai::types::insert_header(base, name, value.clone());
    }
}

/// Merge explicit headers over resolved auth headers, with explicit entries
/// winning per header name (case-insensitively).
fn explicit_headers(
    explicit: Option<&ProviderHeaders>,
    resolved: Option<&ProviderHeaders>,
) -> Option<ProviderHeaders> {
    crate::ai::types::merge_headers(resolved, explicit)
}

/// Apply the models.json `authHeader` flag (upstream
/// `withConfiguredAuth`, provider-composer.ts:374-386): when the model's
/// compat carries `authHeader: true` and a resolved api key exists, the
/// credential is sent as `Authorization: Bearer <apiKey>` instead of (or in
/// addition to) the provider's default header. A caller-supplied
/// `authorization` header still wins. Public for tests/.
pub fn apply_auth_header(
    model: &Model,
    headers: Option<ProviderHeaders>,
    explicit: Option<&ProviderHeaders>,
    api_key: Option<&str>,
) -> Option<ProviderHeaders> {
    let auth_header_enabled = model
        .compat
        .as_ref()
        .and_then(|compat| compat.auth_header)
        .unwrap_or(false);
    if !auth_header_enabled {
        return headers;
    }
    let Some(api_key) = api_key.filter(|key| !key.trim().is_empty()) else {
        return headers;
    };
    let caller_supplied_authorization = explicit
        .into_iter()
        .flat_map(|headers| headers.iter())
        .any(|(name, _)| name.eq_ignore_ascii_case("authorization"));
    if caller_supplied_authorization {
        return headers;
    }
    let mut headers = headers.unwrap_or_default();
    headers.insert("authorization".to_string(), format!("Bearer {api_key}"));
    Some(headers)
}

// ---------------------------------------------------------------------------
// Provider attribution headers (upstream `provider-attribution.ts` +
// `telemetry.ts`)
// ---------------------------------------------------------------------------

const OPENROUTER_HOST: &str = "openrouter.ai";
const NVIDIA_NIM_HOST: &str = "integrate.api.nvidia.com";
const CLOUDFLARE_API_HOST: &str = "api.cloudflare.com";
const CLOUDFLARE_AI_GATEWAY_HOST: &str = "gateway.ai.cloudflare.com";
const OPENCODE_HOST: &str = "opencode.ai";

/// Hostname equality against the request base URL (upstream `matchesHost`);
/// unparseable URLs never match.
fn matches_host(base_url: &str, expected_host: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(|host| host == expected_host))
        .unwrap_or(false)
}

fn is_openrouter_model(model: &Model) -> bool {
    model.provider == "openrouter" || model.base_url.contains(OPENROUTER_HOST)
}

fn is_nvidia_nim_model(model: &Model) -> bool {
    model.provider == "nvidia" || matches_host(&model.base_url, NVIDIA_NIM_HOST)
}

fn is_cloudflare_model(model: &Model) -> bool {
    model.provider == "cloudflare-workers-ai"
        || model.provider == "cloudflare-ai-gateway"
        || matches_host(&model.base_url, CLOUDFLARE_API_HOST)
        || matches_host(&model.base_url, CLOUDFLARE_AI_GATEWAY_HOST)
}

fn is_opencode_model(model: &Model) -> bool {
    model.provider == "opencode"
        || model.provider == "opencode-go"
        || matches_host(&model.base_url, OPENCODE_HOST)
}

/// One cached install-telemetry gate value: the config source it came from
/// plus the stamp of that source's `settings.json` at the time of the read.
struct InstallTelemetryGate {
    /// The resolved agent dir the value was computed for.
    agent_dir: std::path::PathBuf,
    /// Length+mtime of `<agent_dir>/settings.json` when the value was read
    /// (`None` = the file did not exist).
    settings_stamp: Option<crate::config::settings::FileStamp>,
    enabled: bool,
}

/// Settings-derived install-telemetry gate cache (env audit D6).
///
/// **Lifetime / invalidation.** An entry is served only while BOTH match:
/// the resolved agent dir (`PIR_AGENT_DIR` / `PIR_CODING_AGENT_DIR` /
/// `PI_CODING_AGENT_DIR`, else `~/.pi/agent`) and the length+mtime stamp of
/// that dir's `settings.json`. Switching the agent dir therefore re-reads that
/// dir's settings instead of replaying the first dir's value, and a settings
/// edit (including one made outside the process) is picked up on the next
/// call. Only the global scope is consulted here (the load below passes
/// `project_trusted = false`), so the project `.pi` scope cannot change the
/// value and is not part of the stamp.
///
/// The `PI_TELEMETRY` env override is a memory read and wins on every call
/// without touching this cache.
///
/// **Cost.** A hit is one `metadata` call on one small file — a stamp
/// comparison, not the global+project read-and-parse this cache replaced
/// (previously done once per dispatch). A miss re-reads and re-parses.
///
/// **Bound.** Exactly one entry (the last resolved config source); switching
/// agent dirs replaces it rather than accumulating.
static INSTALL_TELEMETRY_CACHE: std::sync::Mutex<Option<InstallTelemetryGate>> =
    std::sync::Mutex::new(None);

/// Number of cached install-telemetry gate values (0 or 1; diagnostic/test
/// seam for the bounded-cache invariant).
pub fn install_telemetry_cache_len() -> usize {
    usize::from(
        INSTALL_TELEMETRY_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some(),
    )
}

/// The install-telemetry gate for attribution headers (upstream
/// `isInstallTelemetryEnabled(settingsManager)`): the `PI_TELEMETRY` env
/// override wins over the `enableInstallTelemetry` setting.
///
/// Ambient boundary: resolves the process agent dir once, then delegates to
/// [`install_telemetry_enabled_in`]. A host that owns its config source (a
/// session built for a specific agent dir) calls that variant directly.
///
/// Upstream evaluates this once at assembly time (sdk.ts wires the resolved
/// flag into the transport). pir keeps the value across requests for the same
/// config source but not across config changes: the cached value is keyed on
/// the resolved agent dir and validated against that dir's `settings.json`
/// stamp (see [`INSTALL_TELEMETRY_CACHE`]), so a changed agent dir or an
/// edited settings file is reflected on the next call.
/// Public for tests/.
pub fn install_telemetry_enabled() -> bool {
    install_telemetry_enabled_in(&crate::coding_agent::context::agent_dir())
}

/// [`install_telemetry_enabled`] against an EXPLICIT agent dir: the setting is
/// read from `<agent_dir>/settings.json` (global scope only), and the cached
/// entry is keyed on that dir plus the file's stamp.
///
/// The `PI_TELEMETRY` override still wins (it is process state, not per-dir
/// settings). The cwd only feeds the never-loaded project scope of the
/// `SettingsManager`, so it stays ambient. Public for tests/.
pub fn install_telemetry_enabled_in(agent_dir: &std::path::Path) -> bool {
    if let Some(over) = crate::config::env::telemetry_override() {
        return over;
    }
    let settings_path = agent_dir.join("settings.json");
    let settings_stamp = crate::config::settings::file_stamp(&settings_path);
    {
        let guard = INSTALL_TELEMETRY_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = guard.as_ref() {
            if cached.agent_dir.as_path() == agent_dir && cached.settings_stamp == settings_stamp {
                return cached.enabled;
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let manager =
        crate::config::settings::SettingsManager::load(agent_dir, &cwd.join(".pi"), &cwd, false);
    let enabled = manager.install_telemetry_enabled();
    // Benign race: concurrent callers compute the same value from the same
    // source, so a last-writer-wins store is indistinguishable from a single
    // write.
    let mut guard = INSTALL_TELEMETRY_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(InstallTelemetryGate {
        agent_dir: agent_dir.to_path_buf(),
        settings_stamp,
        enabled,
    });
    enabled
}

/// The default attribution headers for a provider (upstream
/// `getDefaultAttributionHeaders`), gated on install telemetry being enabled.
/// Public for tests/.
pub fn default_attribution_headers(
    model: &Model,
    telemetry_enabled: bool,
) -> Option<ProviderHeaders> {
    if !telemetry_enabled {
        return None;
    }
    if is_openrouter_model(model) {
        return Some(BTreeMap::from([
            ("HTTP-Referer".to_string(), "https://pi.dev".to_string()),
            ("X-OpenRouter-Title".to_string(), "pi".to_string()),
            (
                "X-OpenRouter-Categories".to_string(),
                "cli-agent".to_string(),
            ),
        ]));
    }
    if is_nvidia_nim_model(model) {
        return Some(BTreeMap::from([(
            "X-BILLING-INVOKE-ORIGIN".to_string(),
            "Pi".to_string(),
        )]));
    }
    if is_cloudflare_model(model) {
        return Some(BTreeMap::from([(
            "User-Agent".to_string(),
            "pi-coding-agent".to_string(),
        )]));
    }
    None
}

/// The opencode session headers (upstream `getSessionHeaders`): sent only for
/// opencode providers with a session id. Public for tests/.
pub fn session_headers(model: &Model, session_id: Option<&str>) -> Option<ProviderHeaders> {
    let session_id = session_id?;
    if !is_opencode_model(model) {
        return None;
    }
    Some(BTreeMap::from([
        ("x-opencode-session".to_string(), session_id.to_string()),
        ("x-opencode-client".to_string(), "pi".to_string()),
    ]))
}

/// Merge the provider attribution headers under the request headers (upstream
/// `mergeProviderAttributionHeaders`): session headers first, attribution
/// defaults over them, then `request_headers` win per header name. Returns
/// `None` when nothing would be sent. Public for tests/.
pub fn merge_provider_attribution_headers(
    model: &Model,
    telemetry_enabled: bool,
    session_id: Option<&str>,
    request_headers: Option<&ProviderHeaders>,
) -> Option<ProviderHeaders> {
    let mut merged = ProviderHeaders::new();
    if let Some(session) = session_headers(model, session_id) {
        merged.extend(session);
    }
    if let Some(attribution) = default_attribution_headers(model, telemetry_enabled) {
        merged.extend(attribution);
    }
    if let Some(request) = request_headers {
        for (name, value) in request {
            merged.insert(name.clone(), value.clone());
        }
    }
    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

// ---------------------------------------------------------------------------
// AI request telemetry (upstream `packages/ai/src/telemetry.ts`)
// ---------------------------------------------------------------------------
//
// The upstream schema (`AI_TELEMETRY_SCHEMA`) is one `pi.ai.request` span per
// logical request — provider/model/api attributes, `pi.ai.first_chunk` and
// `pi.ai.block` events, terminal usage/cost attributes, and the
// request/first-chunk duration histograms plus the token and fractional USD
// cost counters. pir's portable telemetry core (W10-H) provides the
// span/metric/propagation surface; this section is the integration into the
// AI dispatch path.

/// Span name for one logical model request (upstream `pi.ai.request`).
pub const AI_REQUEST_SPAN_NAME: &str = "pi.ai.request";
/// First normalized stream event delivered.
pub const AI_FIRST_CHUNK_EVENT: &str = "pi.ai.first_chunk";
/// A completed content block; content is never recorded.
pub const AI_BLOCK_EVENT: &str = "pi.ai.block";
/// Logical request duration histogram, unit `ms`.
pub const AI_REQUEST_DURATION_METRIC: &str = "pi.ai.request.duration";
/// Time to the first normalized stream event histogram, unit `ms`.
pub const AI_FIRST_CHUNK_DURATION_METRIC: &str = "pi.ai.first_chunk.duration";
/// Reported model token usage counter, unit `token`.
pub const AI_TOKENS_METRIC: &str = "pi.ai.tokens";
/// Reported model cost counter, unit `USD` (upstream `pi.ai.cost`). USD
/// totals are fractional, so this instrument is the f64-valued
/// [`FloatCounter`](crate::telemetry::FloatCounter), not the integer
/// [`Counter`].
pub const AI_COST_METRIC: &str = "pi.ai.cost";

/// Per-request telemetry observer (upstream `observeModelRequest`).
///
/// Created only when [`StreamOptions::telemetry`] holds an enabled parent
/// span. The disabled path is `None`: no span is started, no instrument is
/// created, and the forwarding loops' per-event hook is one `Option` check.
struct ModelRequestObserver {
    span: Span,
    started: Instant,
    provider: String,
    model: String,
    first_chunk_seen: bool,
    /// Accumulated `toolcall_delta` length per content block (UTF-16 units,
    /// matching upstream `delta.length`), emitted on `toolcall_end`.
    tool_chars: BTreeMap<usize, u64>,
    request_duration: Histogram,
    first_chunk_duration: Histogram,
    tokens: Counter,
    cost: crate::telemetry::FloatCounter,
    finished: bool,
}

impl ModelRequestObserver {
    fn start(model: &Model, options: &StreamOptions) -> Option<Self> {
        // A noop parent span means no adapter is installed: stay on the
        // disabled path rather than allocating a span node that records
        // nothing.
        let parent = options.telemetry.as_ref().filter(|span| !span.is_noop())?;
        let telemetry = parent.telemetry();
        let mut attributes = Attributes::new()
            .with("provider", model.provider.clone())
            .with("model", model.id.clone())
            .with("api", model.api.clone());
        if let Some(reasoning) = options.reasoning {
            attributes.insert("reasoning", reasoning.wire_key());
        }
        // Child of the caller's context span on the same recorder (upstream
        // `createTypedSpanStarter(options.telemetry, …)`), so the request
        // joins the caller's trace and keeps its adapter capabilities.
        let span =
            parent.start_child(SpanOptions::new(AI_REQUEST_SPAN_NAME).with_attributes(attributes));
        Some(Self {
            span,
            started: Instant::now(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            first_chunk_seen: false,
            tool_chars: BTreeMap::new(),
            request_duration: telemetry.histogram(
                AI_REQUEST_DURATION_METRIC,
                &HistogramOptions::new()
                    .with_description("Logical request duration.")
                    .with_unit("ms"),
            ),
            first_chunk_duration: telemetry.histogram(
                AI_FIRST_CHUNK_DURATION_METRIC,
                &HistogramOptions::new()
                    .with_description("Time to the first normalized stream event.")
                    .with_unit("ms"),
            ),
            tokens: telemetry.counter(
                AI_TOKENS_METRIC,
                &InstrumentOptions::new()
                    .with_description("Reported model token usage.")
                    .with_unit("token"),
            ),
            cost: telemetry.float_counter(
                AI_COST_METRIC,
                &InstrumentOptions::new()
                    .with_description("Reported model cost.")
                    .with_unit("USD"),
            ),
            finished: false,
        })
    }

    /// Observe one normalized stream event (upstream `onEvent`).
    fn observe(&mut self, event: &AssistantMessageEvent) {
        if self.finished {
            return;
        }
        if !self.first_chunk_seen {
            self.first_chunk_seen = true;
            self.span
                .add_event(AI_FIRST_CHUNK_EVENT, &Attributes::EMPTY);
            self.first_chunk_duration
                .record(self.elapsed_ms(), &self.identity_attributes());
        }
        match event {
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                ..
            } => self.record_block("text", *content_index, utf16_len(content), None),
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                ..
            } => self.record_block("thinking", *content_index, utf16_len(content), None),
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                ..
            } => {
                let chars = self.tool_chars.entry(*content_index).or_insert(0);
                *chars = chars.saturating_add(utf16_len(delta));
            }
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                ..
            } => {
                let chars = self.tool_chars.remove(content_index).unwrap_or(0);
                self.record_block(
                    "tool_call",
                    *content_index,
                    chars,
                    Some(tool_call.name.as_str()),
                );
            }
            AssistantMessageEvent::Done { message, .. }
            | AssistantMessageEvent::Error { error: message, .. } => {
                self.finish_with_message(message)
            }
            _ => {}
        }
    }

    /// Record a completed content block; block content is never captured.
    fn record_block(&self, block_type: &'static str, index: usize, chars: u64, tool: Option<&str>) {
        let mut attributes = Attributes::new()
            .with("type", block_type)
            .with(
                "index",
                count_attr(u64::try_from(index).unwrap_or(u64::MAX)),
            )
            .with("chars", count_attr(chars));
        if let Some(tool) = tool {
            attributes.insert("tool", tool.to_owned());
        }
        self.span.add_event(AI_BLOCK_EVENT, &attributes);
    }

    /// Terminal event (upstream `onResult`): end attributes, bounded error
    /// status, one request-duration measurement, and the token counter.
    fn finish_with_message(&mut self, message: &AssistantMessage) {
        if self.finished {
            return;
        }
        self.finished = true;
        let stop = message.stop_reason.to_string();
        let mut attributes = Attributes::new();
        attributes.insert("stop", stop.clone());
        attributes.insert("usage.input", count_attr(message.usage.input));
        attributes.insert("usage.output", count_attr(message.usage.output));
        attributes.insert("usage.cache_read", count_attr(message.usage.cache_read));
        attributes.insert("usage.cache_write", count_attr(message.usage.cache_write));
        attributes.insert("cost.total", message.usage.cost.total);
        if let Some(response_id) = &message.response_id {
            attributes.insert("response.id", response_id.clone());
        }
        self.span.set_attributes(&attributes);
        if message.stop_reason == StopReason::Error {
            // Bounded error status (upstream `errorMessage?.slice(0, 500)`).
            let error = message
                .error_message
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(500)
                .collect::<String>();
            self.span.set_status(&SpanStatus::Error {
                error: Some(SpanError::new("provider_error", error)),
            });
        }
        let mut stop_attributes = self.identity_attributes();
        stop_attributes.insert("stop", stop);
        self.request_duration
            .record(self.elapsed_ms(), &stop_attributes);

        let identity = self.identity_attributes();
        self.tokens
            .add(message.usage.input, &identity.clone().with("kind", "input"));
        self.tokens.add(
            message.usage.output,
            &identity.clone().with("kind", "output"),
        );
        self.tokens.add(
            message.usage.cache_read,
            &identity.clone().with("kind", "cache_read"),
        );
        self.tokens.add(
            message.usage.cache_write,
            &identity.clone().with("kind", "cache_write"),
        );
        // Upstream records `pi.ai.cost` (unit USD) through a numeric counter;
        // the value is fractional, so it goes through the f64-valued
        // `FloatCounter` instead of the integer token counter.
        self.cost.add(message.usage.cost.total, &identity);
        self.span.end();
    }

    /// End the span without a terminal message (upstream
    /// `observeResult(undefined)`): no duration histogram is recorded.
    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.span.end();
    }

    fn identity_attributes(&self) -> Attributes {
        Attributes::new()
            .with("provider", self.provider.clone())
            .with("model", self.model.clone())
    }

    fn elapsed_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }
}

/// Inject the request span's W3C trace context into provider request headers
/// (upstream spans expose `propagation.inject(span)`).
///
/// `None`/invalid context is the disabled path: the caller's header set is
/// returned untouched, with no allocation and no header added. The W3C
/// `traceparent`/`tracestate` rendering is [`W3cPropagator`]'s — this function
/// never parses or formats trace context itself.
pub fn inject_trace_headers(
    headers: Option<ProviderHeaders>,
    context: Option<&TraceContext>,
) -> Option<ProviderHeaders> {
    let Some(context) = context.filter(|context| context.is_valid()) else {
        return headers;
    };
    // `Carrier` and `ProviderHeaders` are both `BTreeMap<String, String>`.
    let mut carrier = ProviderHeaders::new();
    if !W3cPropagator.inject(context, &mut carrier) {
        return headers;
    }
    let mut headers = headers.unwrap_or_default();
    headers.extend(carrier);
    Some(headers)
}

/// Read the W3C trace context a caller sent on provider headers (the extract
/// half of propagation; upstream `TelemetryPropagation.extract`).
///
/// `None` when no valid `traceparent` is present. A host can turn this into a
/// parent [`Span`] via
/// [`Telemetry::start_span`](crate::telemetry::Telemetry::start_span) with
/// [`SpanOptions::with_parent`].
pub fn extract_trace_context(headers: &ProviderHeaders) -> Option<TraceContext> {
    W3cPropagator.extract(headers)
}

/// JS `String.length` semantics (UTF-16 code units), matching the upstream
/// `chars` event attribute.
fn utf16_len(value: &str) -> u64 {
    u64::try_from(value.encode_utf16().count()).unwrap_or(u64::MAX)
}

/// u64 counter value as a telemetry attribute.
fn count_attr(value: u64) -> AttributeValue {
    AttributeValue::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

/// One content block accumulated from a stream's delta events.
enum StreamedBlock {
    Text(String),
    Thinking(String),
    ToolCall(crate::ai::types::ToolCall),
}

/// Carries the accumulated streamed content into the terminal message of a
/// failed/aborted stream.
///
/// Upstream keeps the parser's accumulated output on failure: the adapter's
/// `catch` block mutates the SAME object whose content was streamed —
/// `output.stopReason = signal.aborted ? "aborted" : "error"`, `errorMessage`
/// set, `content` untouched (`pi/packages/ai/src/api/openai-completions.ts:
/// 708-730`; the same shape in `anthropic-messages.ts:923-932`,
/// `bedrock-converse-stream.ts:345-355`, `google-generative-ai.ts:288-296`,
/// `mistral-conversations.ts:165-177`, `azure-openai-responses.ts:112-124`).
/// pir's adapter-side finalizer (`api::finish_provider_stream`) only sees the
/// sink — the parser lives inside each adapter's stream function — so it can
/// build nothing but an empty message.
///
/// Every adapter's events pass through the registry forwarding loops that use
/// this type, so accumulating the block content from those events gives the
/// FINAL message of a failed stream the text/live blocks the provider had
/// produced, with no change to the streaming events themselves. The
/// accumulation deliberately rebuilds the blocks from the delta protocol
/// instead of retaining the previous event's `partial` snapshot: a retained
/// `Arc` would keep the parser's accumulator shared, turning every subsequent
/// delta into a full-message `Arc::make_mut` copy — the exact O(n²) the
/// parsers' copy-on-write accumulation exists to avoid.
#[derive(Default)]
struct StreamedContent {
    /// Blocks by content index (BTreeMap keeps the protocol's order).
    blocks: BTreeMap<usize, StreamedBlock>,
}

impl StreamedContent {
    /// Record one streamed delta / finalized block. Terminal events and the
    /// `Start` snapshot carry nothing to accumulate.
    fn observe(&mut self, event: &AssistantMessageEvent) {
        match event {
            AssistantMessageEvent::TextStart { content_index, .. } => {
                self.blocks
                    .entry(*content_index)
                    .or_insert_with(|| StreamedBlock::Text(String::new()));
            }
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                ..
            } => {
                if let StreamedBlock::Text(text) = self
                    .blocks
                    .entry(*content_index)
                    .or_insert_with(|| StreamedBlock::Text(String::new()))
                {
                    text.push_str(delta);
                }
            }
            // `text_end` carries the block's full text (upstream
            // `ensureTextBlock` finalization).
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                ..
            } => {
                self.blocks
                    .insert(*content_index, StreamedBlock::Text(content.clone()));
            }
            AssistantMessageEvent::ThinkingStart { content_index, .. } => {
                self.blocks
                    .entry(*content_index)
                    .or_insert_with(|| StreamedBlock::Thinking(String::new()));
            }
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                ..
            } => {
                if let StreamedBlock::Thinking(thinking) = self
                    .blocks
                    .entry(*content_index)
                    .or_insert_with(|| StreamedBlock::Thinking(String::new()))
                {
                    thinking.push_str(delta);
                }
            }
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                ..
            } => {
                self.blocks
                    .insert(*content_index, StreamedBlock::Thinking(content.clone()));
            }
            // Only a finalized call has complete arguments; an in-progress
            // call has none to execute and is dropped (the aborted turn never
            // runs tools, and upstream deletes the streaming scratch fields
            // before persisting).
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                ..
            } => {
                self.blocks
                    .insert(*content_index, StreamedBlock::ToolCall(tool_call.clone()));
            }
            AssistantMessageEvent::ToolCallStart { .. }
            | AssistantMessageEvent::ToolCallDelta { .. }
            | AssistantMessageEvent::Start { .. }
            | AssistantMessageEvent::Done { .. }
            | AssistantMessageEvent::Error { .. } => {}
        }
    }

    /// The accumulated blocks in content order; empty text/thinking blocks
    /// (started but never streamed) are omitted.
    fn content(&self) -> Vec<crate::ai::types::ContentBlock> {
        use crate::ai::types::{ContentBlock, TextContent, ThinkingContent};
        self.blocks
            .values()
            .filter_map(|block| match block {
                StreamedBlock::Text(text) if !text.is_empty() => {
                    Some(ContentBlock::Text(TextContent {
                        text: text.clone(),
                        text_signature: None,
                    }))
                }
                StreamedBlock::Thinking(thinking) if !thinking.is_empty() => {
                    Some(ContentBlock::Thinking(ThinkingContent {
                        thinking: thinking.clone(),
                        thinking_signature: None,
                        redacted: false,
                    }))
                }
                StreamedBlock::ToolCall(call) => Some(ContentBlock::ToolCall(call.clone())),
                _ => None,
            })
            .collect()
    }

    /// Return `event` with a terminal `Error` aligned to the stream outcome:
    /// a user abort records `stop_reason: aborted`, a transport/server failure
    /// `stop_reason: error` (upstream maps both from the abort signal), and an
    /// EMPTY terminal message adopts the accumulated content. A message that
    /// already carries its own content (the proxy decoder, pi-messages) and
    /// every successful `Done` event are left untouched.
    fn finalize(&self, event: AssistantMessageEvent) -> AssistantMessageEvent {
        match event {
            AssistantMessageEvent::Error { reason, mut error } => {
                let stop_reason = match reason {
                    ErrorReason::Aborted => StopReason::Aborted,
                    ErrorReason::Error => StopReason::Error,
                };
                if error.content.is_empty() {
                    let content = self.content();
                    if !content.is_empty() {
                        error.content = content;
                    }
                }
                error.stop_reason = stop_reason;
                AssistantMessageEvent::Error { reason, error }
            }
            other => other,
        }
    }
}

/// Resolve the model-derived defaults every upstream API applies in its
/// `streamSimple` wrapper: clamp the requested thinking level to the levels
/// the model supports (`getSupportedThinkingLevels`/`clampThinkingLevel`).
///
/// The budget/`max_tokens` coupling stays in the adapters — each owns the
/// provider-specific rule (Anthropic adaptive vs. budget-based, Bedrock
/// `additionalModelRequestFields`, OpenAI reasoning token budgets) — and reads
/// the same [`StreamOptions::thinking_budgets`] this helper leaves untouched.
pub fn simple_stream_options(model: &Model, mut options: StreamOptions) -> StreamOptions {
    if let Some(level) = options.reasoning {
        options.reasoning = Some(crate::ai::types::clamp_thinking_level(model, level));
    }
    options
}

/// Tokens always reserved for the answer when clamping `max_tokens` to the
/// remaining context (upstream `simple-options.ts` `CONTEXT_SAFETY_TOKENS`).
pub const CONTEXT_SAFETY_TOKENS: u64 = 4096;

/// Floor for a context-clamped `max_tokens` (upstream `MIN_MAX_TOKENS`).
pub const MIN_MAX_TOKENS: u64 = 1;

/// Clamp a requested `max_tokens` to the context window left after the
/// estimated transcript (upstream `clampMaxTokensToContext`,
/// `simple-options.ts:18-22`):
/// `min(maxTokens, max(1, contextWindow - estimateContextTokens - 4096))`.
/// A non-positive context window disables the clamp (the model declares no
/// limit), matching upstream's early return. Public for tests/.
pub fn clamp_max_tokens_to_context(model: &Model, context: &Context, max_tokens: u64) -> u64 {
    if model.context_window == 0 {
        return max_tokens.max(MIN_MAX_TOKENS);
    }
    let estimated = crate::compaction::estimate_context_tokens(&context.messages).tokens;
    let available = model
        .context_window
        .saturating_sub(estimated)
        .saturating_sub(CONTEXT_SAFETY_TOKENS);
    max_tokens.min(available.max(MIN_MAX_TOKENS))
}

/// [`simple_stream_options`] plus the context clamp every simple entry applies
/// (upstream `buildBaseOptions`: `maxTokens: clampMaxTokensToContext(model,
/// context, options?.maxTokens ?? model.maxTokens)`). The resulting
/// `max_tokens` is always explicit, so an adapter cannot fall back to the
/// unclamped `model.max_tokens`.
pub fn simple_stream_options_for_context(
    model: &Model,
    context: &Context,
    options: StreamOptions,
) -> StreamOptions {
    let mut options = simple_stream_options(model, options);
    let requested = options.max_tokens.unwrap_or(model.max_tokens);
    options.max_tokens = Some(clamp_max_tokens_to_context(model, context, requested));
    options
}

/// Apply model-rate cost to a terminal stream event's message when the
/// provider did not report a cost (upstream adapters call `calculateCost`
/// when usage is finalized; the terminal message is the usage record every
/// consumer — footer totals, session persistence — reads).
///
/// A provider-billed total (`totalFromProvider == true`, e.g. OpenRouter's
/// accounting) is never overwritten by the catalog estimate — even when the
/// billed total happens to be zero.
fn apply_model_cost(model: &Model, event: AssistantMessageEvent) -> AssistantMessageEvent {
    let compute = |message: &mut AssistantMessage| {
        if message.usage.cost.total == 0.0 && !message.usage.cost.is_provider_billed() {
            calculate_cost(model, &mut message.usage);
        }
    };
    match event {
        AssistantMessageEvent::Done {
            reason,
            mut message,
        } => {
            compute(&mut message);
            AssistantMessageEvent::Done { reason, message }
        }
        AssistantMessageEvent::Error { reason, mut error } => {
            compute(&mut error);
            AssistantMessageEvent::Error { reason, error }
        }
        other => other,
    }
}

/// Build an error `AssistantMessage` for the given model.
pub(crate) fn error_message(model: &Model, message: &str) -> AssistantMessage {
    AssistantMessage {
        role: crate::ai::types::AssistantRole::Assistant,
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: Default::default(),
        stop_reason: StopReason::Error,
        deferred: None,
        error_message: Some(message.to_string()),
        raw_stop_reason: None,
        end_turn: None,
        provider_thinking_level: None,
        thinking_level: None,
        duration_ms: None,
        timestamp: crate::ai::util::now_ms(),
    }
}
