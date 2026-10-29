//! Dynamic provider registration (extension-facing, upstream registerProvider)
//! plus the periodic remote catalog overlay (upstream
//! `withRemoteCatalog`/`remote-catalog-provider.ts`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Deserialize;

use super::construction::build_provider_from_models;
use super::models_store::{read_models_store_entry, write_models_store_entry, ModelsStoreEntry};
use crate::ai::error::{AiError, AiResult};
use crate::ai::provider::{ProviderBox, ProviderRegistry};
use crate::ai::types::{Model, ModelCost, ModelCostRates, ModelCostTier, ModelKind, OpenAiCompat};

/// Availability-refresh callback for dynamic providers. Returns the up-to-date
/// model list; an empty list or error keeps the previously known models.
pub type RefreshFn = Arc<dyn Fn() -> AiResult<Vec<Model>> + Send + Sync>;

/// Configuration for a runtime-registered provider (the extension-facing
/// `registerProvider` entry; Phase 4 extension runtime consumes this).
#[derive(Clone)]
pub struct DynamicProviderConfig {
    pub base_url: String,
    /// Static API key; stored in the registry's credential store when set.
    pub api_key: Option<String>,
    /// Wire API (`openai-chat`, `anthropic-messages`, or
    /// `google-generative-ai`).
    pub api: String,
    pub models: Vec<Model>,
    /// Provider-level compat knobs merged under per-model compat.
    pub compat: Option<OpenAiCompat>,
    /// Optional one-shot availability refresh, invoked at registration.
    pub refresh: Option<RefreshFn>,
}

impl Default for DynamicProviderConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: None,
            api: "openai-chat".to_string(),
            models: Vec::new(),
            compat: None,
            refresh: None,
        }
    }
}

impl DynamicProviderConfig {
    /// Normalize models: stamp provider id/base url and merge compat.
    fn normalized_models(&self, provider_id: &str) -> Vec<Model> {
        self.models
            .iter()
            .map(|m| Model {
                provider: provider_id.to_string(),
                base_url: if m.base_url.is_empty() {
                    self.base_url.clone()
                } else {
                    m.base_url.clone()
                },
                api: if m.api.is_empty() {
                    self.api.clone()
                } else {
                    m.api.clone()
                },
                compat: self
                    .compat
                    .as_ref()
                    .map(|provider_compat| provider_compat.merged_with(m.compat.as_ref()))
                    .or_else(|| m.compat.clone()),
                ..m.clone()
            })
            .collect()
    }
}

/// Dynamic provider registration for [`ProviderRegistry`]
/// (upstream `registerProvider`). A trait so the registry type itself stays in
/// `crate::ai::provider`.
pub trait RegisterDynamicProvider {
    /// Register (or replace) a runtime provider built from `config`.
    fn register_dynamic_provider(
        &mut self,
        name: impl Into<String>,
        config: DynamicProviderConfig,
    ) -> AiResult<ProviderBox>;
}

impl RegisterDynamicProvider for ProviderRegistry {
    fn register_dynamic_provider(
        &mut self,
        name: impl Into<String>,
        config: DynamicProviderConfig,
    ) -> AiResult<ProviderBox> {
        let provider_id = name.into();
        // One-shot availability refresh: a successful non-empty result
        // replaces the configured model list. (Periodic revalidation for
        // runtime-registered providers is the [`RemoteCatalogOverlay`]'s
        // job — see its notes for the upstream etag-revalidation parity.)
        let models = match &config.refresh {
            Some(refresh) => match refresh() {
                Ok(refreshed) if !refreshed.is_empty() => {
                    let with_refresh = DynamicProviderConfig {
                        models: refreshed,
                        ..config
                    };
                    with_refresh.normalized_models(&provider_id)
                }
                _ => config.normalized_models(&provider_id),
            },
            None => config.normalized_models(&provider_id),
        };
        if models.is_empty() {
            return Err(AiError::UnknownProvider(format!(
                "dynamic provider {provider_id} has no models"
            )));
        }
        // `config.api_key` is intentionally not written to the registry's
        // credential store here: `ProviderRegistry` exposes no public write
        // path in this slice. Callers pass it per-request via
        // `StreamOptions::api_key` or persist it in their own credential
        // store (the Phase 4 extension runtime wires this up).
        let display_name = provider_id.clone();
        let provider = build_provider_from_models(&provider_id, &display_name, &models)?;
        self.set_provider(provider.clone());
        Ok(provider)
    }
}

/// Shared (interior-mutable) dynamic provider registration for a
/// `Arc<ProviderRegistry>` — the seam the extension runtime's
/// `registerProvider` uses. Dynamic entries live in their own overlay map
/// and win over statically registered providers on every lookup; they are
/// also returned by `providers()` / `get_models()` / `get_model`.
pub trait RegisterDynamicProviderShared {
    /// Register (or replace) a runtime provider built from `config` without
    /// requiring `&mut` access to the registry.
    fn register_dynamic_provider_shared(
        &self,
        name: impl Into<String>,
        config: DynamicProviderConfig,
    ) -> AiResult<ProviderBox>;

    /// Remove a dynamic provider previously registered through
    /// [`Self::register_dynamic_provider_shared`]. Returns whether an entry
    /// was removed. Statically registered providers are untouched.
    fn unregister_dynamic_provider_shared(&self, name: &str) -> bool;
}

impl RegisterDynamicProviderShared for ProviderRegistry {
    fn register_dynamic_provider_shared(
        &self,
        name: impl Into<String>,
        config: DynamicProviderConfig,
    ) -> AiResult<ProviderBox> {
        // Reuse the shared construction path by registering into a throwaway
        // overlay: build the provider directly to avoid mutating `providers`.
        let provider_id = name.into();
        let models = config.normalized_models(&provider_id);
        if models.is_empty() {
            return Err(AiError::UnknownProvider(format!(
                "dynamic provider {provider_id} has no models"
            )));
        }
        let display_name = provider_id.clone();
        let provider = build_provider_from_models(&provider_id, &display_name, &models)?;
        self.dynamic
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider_id, provider.clone());
        Ok(provider)
    }

    fn unregister_dynamic_provider_shared(&self, name: &str) -> bool {
        self.dynamic
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name)
            .is_some()
    }
}

// ---------------------------------------------------------------------------
// Periodic remote catalog overlay (upstream `withRemoteCatalog`)
// ---------------------------------------------------------------------------
//
// Upstream wraps static built-in providers with a remote catalog overlay
// (`packages/coding-agent/src/core/remote-catalog-provider.ts`): a background
// refresh revalidates the remote catalog with `If-None-Match`, keeps the
// cached overlay on 304/transient failure, and swaps in a freshly downloaded
// catalog on 200 while persisting the validator in `models-store.json`.
// pir implements the same lifecycle here against the shared registry's
// dynamic-provider map (dynamic entries win over statically registered
// providers on every lookup, so readers never block on a refresh — the
// RwLock critical sections below only touch the small overlay state, and the
// registry lookup clones an `Arc`).

/// Revalidation cadence for the remote catalog overlay (upstream
/// `remote-catalog-provider.ts`
/// `REMOTE_CATALOG_REFRESH_INTERVAL_MS = 4h`); overridable per overlay via
/// [`RemoteCatalogOverlayConfig::interval`].
pub const REMOTE_CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);

/// Per-attempt request timeout (upstream `REMOTE_CATALOG_ATTEMPT_TIMEOUT_MS`).
pub const REMOTE_CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

/// Maximum accepted catalog body size (bounded reads; upstream bounds the
/// fetch the same way through its HTTP client).
pub const REMOTE_CATALOG_MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Whether a remote catalog is newer than the built-in model data shipped with
/// pir — the upstream `remote-catalog-provider.ts` `isNewer` predicate:
///
/// ```text
/// localGeneratedAt === undefined || (lastModified !== undefined && lastModified > localGeneratedAt)
/// ```
///
/// Both timestamps are Unix **milliseconds** (upstream parses them with
/// `Date.parse`: the catalog manifest's `generatedAt` and the response's
/// `Last-Modified` header).
///
/// `local_generated_at == None` is the compatibility path: an overlay that has
/// no generation timestamp applies the remote catalog unconditionally,
/// including a response without `Last-Modified`. With a known local timestamp
/// the remote catalog must be strictly newer; an equal timestamp and a missing
/// `Last-Modified` both count as "not newer", so the built-in catalog stays in
/// effect and the stale remote overlay is dropped.
pub fn remote_catalog_is_newer(
    last_modified: Option<u64>,
    local_generated_at: Option<u64>,
) -> bool {
    match local_generated_at {
        None => true,
        Some(local) => last_modified.is_some_and(|remote| remote > local),
    }
}

/// Default remote catalog base URL (upstream `remote-catalog-provider.ts`
/// `DEFAULT_CATALOG_BASE_URL`). Overridable with `PI_CATALOG_BASE_URL`.
pub const REMOTE_CATALOG_DEFAULT_BASE_URL: &str = "https://pi.dev";

/// A credential-specific catalog transform (upstream
/// `remote-catalog-provider.ts` `PersonalizeCatalog`): the merged catalog
/// (built-in plus a newer pi.dev catalog) goes in, the personalized model
/// list plus its models-store wire values — the offline cache — come out.
pub type PersonalizeCatalog = dyn Fn(
        Vec<Model>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = AiResult<(Vec<Model>, Vec<serde_json::Value>)>> + Send,
        >,
    > + Send
    + Sync;

/// Configuration for one provider's remote catalog overlay.
#[derive(Debug, Clone)]
pub struct RemoteCatalogOverlayConfig {
    /// The provider whose catalog is overlaid (must match the built-in
    /// catalog id for the baseline merge to apply).
    pub provider_id: String,
    /// Absolute URL of the remote catalog document.
    pub catalog_url: String,
    /// Agent dir for the `models-store.json` ETag/models persistence.
    pub agent_dir: PathBuf,
    /// Revalidation interval override (`None` = the 4h upstream default).
    pub interval: Option<Duration>,
}

/// One remote catalog model entry (the upstream `Model` wire shape, same
/// parser contract as the embedded catalog loader in `catalog.rs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteCatalogModel {
    #[serde(default)]
    id: String,
    /// Upstream model-type discrimination; only chat models are overlaid.
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    #[serde(default)]
    input: Option<Vec<String>>,
    #[serde(default)]
    cost: Option<RemoteCatalogCost>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    compat: Option<OpenAiCompat>,
}

/// Remote catalog cost block (flat rates plus optional prompt-length tiers).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RemoteCatalogCost {
    #[serde(default)]
    input: f64,
    #[serde(default)]
    output: f64,
    #[serde(default)]
    cache_read: f64,
    #[serde(default)]
    cache_write: f64,
    #[serde(default)]
    tiers: Vec<RemoteCatalogCostTier>,
}

/// A remote catalog cost tier.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteCatalogCostTier {
    #[serde(default)]
    input: f64,
    #[serde(default)]
    output: f64,
    #[serde(default)]
    cache_read: f64,
    #[serde(default)]
    cache_write: f64,
    input_tokens_above: u64,
}

/// The mutable overlay state; guarded by a short-critical-section RwLock.
#[derive(Default)]
struct OverlayState {
    /// Validator from the remote catalog's `ETag` header, stored verbatim
    /// (quotes included) and echoed back as `If-None-Match`.
    etag: Option<String>,
    /// `Last-Modified` of the newest remote catalog response, in Unix
    /// milliseconds (upstream `ModelsStoreEntry.lastModified`, which is
    /// `Date.parse` output). `None` until a response is seen; persisted so the
    /// generation gate applies across restarts and survives 304/transient
    /// passes.
    last_modified: Option<u64>,
    /// The overlaid remote models (empty = no active overlay).
    remote_models: Vec<Model>,
    /// The raw wire-shape values behind `remote_models`, persisted verbatim
    /// to `models-store.json` so a restore keeps the upstream camelCase
    /// fidelity.
    remote_values: Vec<serde_json::Value>,
}

/// A periodic remote catalog overlay for one provider (upstream
/// `withRemoteCatalog`): restores the persisted overlay from
/// `models-store.json`, then revalidates it on a fixed interval with
/// `If-None-Match`. On 304 the local overlay is kept; on transient failure
/// the cached body and its validator stay valid (silent degradation with a
/// typed `tracing` log); on 200 the overlay is swapped in atomically
/// (registry-side short critical section, no reader blocking) and the ETag is
/// persisted for the next cycle. Every applied catalog — restored or freshly
/// downloaded — must be newer than the embedded catalog's `generatedAt`
/// (upstream `isNewer` via the response `Last-Modified`); a stale catalog is
/// cached but never registered, so the built-in model data stays authoritative.
/// 404/501 clear the overlay so the baseline catalog shows through again.
pub struct RemoteCatalogOverlay {
    config: RemoteCatalogOverlayConfig,
    registry: Arc<ProviderRegistry>,
    /// Baseline models from the built-in catalog (remote entries replace
    /// these by id, others are appended — upstream `mergeModels`).
    baseline: Vec<Model>,
    display_name: String,
    /// A credential-specific catalog transform (upstream
    /// `withPersonalizedCatalog`): when set, every refresh runs the merged
    /// catalog through it, the result replaces the provider's model list and
    /// is persisted (offline cache).
    personalize: Option<Arc<PersonalizeCatalog>>,
    /// Local catalog generation timestamp (Unix ms) for the upstream
    /// `Last-Modified` gate ([`remote_catalog_is_newer`]). Production overlays
    /// use the embedded catalog's manifest timestamp; `None` disables the gate
    /// (compatibility path for overlays without a local generation time).
    local_generated_at: Option<u64>,
    state: RwLock<OverlayState>,
    spawned: AtomicBool,
}

impl RemoteCatalogOverlay {
    /// Build an overlay for `config`, snapshotting the built-in catalog
    /// baseline for the provider id (empty when the id has no catalog entry).
    pub fn new(
        config: RemoteCatalogOverlayConfig,
        registry: Arc<ProviderRegistry>,
    ) -> AiResult<Self> {
        Self::new_personalized(config, registry, None)
    }

    /// [`RemoteCatalogOverlay::new`] with a personalizer (upstream
    /// `withPersonalizedCatalog`): the merged catalog is passed through it on
    /// every refresh and the personalized list replaces the provider's
    /// catalog, including offline.
    pub fn new_personalized(
        config: RemoteCatalogOverlayConfig,
        registry: Arc<ProviderRegistry>,
        personalize: Option<Arc<PersonalizeCatalog>>,
    ) -> AiResult<Self> {
        let (baseline, display_name) = match super::catalog::load_provider(&config.provider_id)? {
            Some(provider) => (provider.models, provider.display_name),
            None => (Vec::new(), config.provider_id.clone()),
        };
        Ok(Self {
            config,
            registry,
            baseline,
            display_name,
            personalize,
            // Upstream `model-runtime.ts` wraps every built-in provider with
            // the manifest's `generatedAt` (upstream
            // `getBuiltinModelDataGeneratedAt`); the same timestamp gates the
            // remote catalogs here.
            local_generated_at: super::catalog::builtin_catalog_generated_at(),
            state: RwLock::new(OverlayState::default()),
            spawned: AtomicBool::new(false),
        })
    }

    /// Override the local catalog generation timestamp used by the upstream
    /// `Last-Modified` gate ([`remote_catalog_is_newer`]). `None` selects the
    /// compatibility path (no local timestamp: every remote catalog applies,
    /// including responses without `Last-Modified`). Intended for tests and for
    /// overlays built for providers without a generation timestamp.
    pub fn with_local_generated_at(mut self, local_generated_at: Option<u64>) -> Self {
        self.local_generated_at = local_generated_at;
        self
    }

    /// [`RemoteCatalogOverlay::new`] with the provider's built-in
    /// personalizer applied ([`personalize_for`]; upstream
    /// `model-runtime.ts` wraps OpenRouter with `withPersonalizedCatalog`).
    /// Every construction path — the session-runtime install and the strict
    /// `update --models` refresh — goes through here, so they share one
    /// personalizer.
    pub fn new_with_provider_personalizer(
        config: RemoteCatalogOverlayConfig,
        registry: Arc<ProviderRegistry>,
    ) -> AiResult<Self> {
        let personalize = personalize_for(&config.provider_id);
        Self::new_personalized(config, registry, personalize)
    }

    /// A snapshot of the current overlay models (never blocks a refresh for
    /// long: clone under a short read lock).
    pub fn remote_models(&self) -> Vec<Model> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remote_models
            .clone()
    }

    /// The currently stored validator, verbatim.
    pub fn etag(&self) -> Option<String> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .etag
            .clone()
    }

    /// Spawn the background revalidation task (tokio interval). Returns
    /// `false` when a task was already spawned for this overlay. The persisted
    /// catalog is restored synchronously first (the cache-only phase); network
    /// revalidation keeps its delayed first hop. Failures are logged and
    /// swallowed: the overlay keeps its last known state.
    pub fn spawn_refresh_task(self: &Arc<Self>) -> bool {
        // Phase 1 of the refresh contract (upstream's offline/cache-only
        // pass): apply the persisted catalog NOW, before the delayed network
        // revalidation. Running it only from the first background tick left a
        // session on the embedded baseline for a whole interval (4h) even
        // though `models-store.json` held a newer catalog, which defeats the
        // persisted overlay across restarts.
        self.restore_persisted_overlay();
        // Claim first, but only from inside a runtime: `tokio::spawn` panics
        // without one, and a claim set before that panic would permanently
        // disable background refresh for this overlay.
        if tokio::runtime::Handle::try_current().is_err() {
            return false;
        }
        if self.spawned.swap(true, Ordering::SeqCst) {
            return false;
        }
        let overlay = Arc::clone(self);
        let interval = self
            .config
            .interval
            .unwrap_or(REMOTE_CATALOG_REFRESH_INTERVAL)
            .max(Duration::from_millis(1));
        // Upstream checks the persisted `checkedAt` and skips an immediate
        // revalidation while the stored check is younger than the refresh
        // interval (`stored.checkedAt < interval -> skip`). Without reading it
        // a restart always waited one full interval, even when the persisted
        // overlay had just been checked. No persisted timestamp keeps the
        // legacy delayed first hop, so a fresh install does not fan out one
        // request per provider at startup.
        let checked_at = read_models_store_entry(&self.config.agent_dir, &self.config.provider_id)
            .and_then(|entry| entry.checked_at);
        let first_delay =
            remote_catalog_initial_delay(interval, checked_at, crate::ai::util::now_ms());
        tokio::spawn(async move {
            tokio::time::sleep(first_delay).await;
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                if let Err(error) = overlay.refresh_once().await {
                    tracing::warn!(
                        provider = %overlay.config.provider_id,
                        %error,
                        "remote catalog overlay refresh failed; keeping last known state"
                    );
                }
            }
        });
        true
    }

    /// One revalidation pass: restore the persisted overlay when this is the
    /// first pass, then hit the remote catalog. Returns whether the overlay
    /// content changed (seed, update, or clear). Transport errors and error
    /// statuses degrade silently (the production background path).
    pub async fn refresh_once(&self) -> AiResult<bool> {
        self.refresh_once_inner(false).await
    }

    /// Strict revalidation pass for the explicit `update --models` path
    /// (upstream `remote-catalog-provider.ts` throwing
    /// `Model catalog request failed for {provider}: {status}` on a non-OK
    /// status other than 404/501, with transport errors surfaced into the
    /// aggregate by `models.ts` `refresh`): transport errors and error
    /// statuses return `Err` instead of degrading silently. 404/501 still
    /// clear the overlay and report success (upstream treats them as "the
    /// catalog no longer serves this provider", i.e. a skip), and 304 keeps
    /// the overlay. The persisted `checkedAt` is still moved before an error
    /// is returned, matching the upstream publish-then-throw order.
    pub async fn refresh_once_strict(&self) -> AiResult<bool> {
        self.refresh_once_inner(true).await
    }

    async fn refresh_once_inner(&self, strict: bool) -> AiResult<bool> {
        let changed = self.restore_persisted_overlay();
        let (etag, has_cached_body) = {
            let state = self
                .state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (state.etag.clone(), !state.remote_models.is_empty())
        };
        // A personalized overlay skips the interval/etag revalidation: the
        // per-key transform must run on every refresh (upstream
        // `withPersonalizedCatalog` always issues a full catalog request).
        let has_cached_body = has_cached_body && self.personalize.is_none();

        let client = reqwest::Client::builder()
            .timeout(REMOTE_CATALOG_REQUEST_TIMEOUT)
            .connect_timeout(REMOTE_CATALOG_REQUEST_TIMEOUT)
            .build()
            .map_err(|error| AiError::Http(error.to_string()))?;
        let mut request = client
            .get(&self.config.catalog_url)
            .header("accept", "application/json");
        // Only revalidate when a cached body backs the validator, so a 304
        // can never leave the overlay empty (upstream remote-catalog-provider).
        if has_cached_body {
            if let Some(etag) = &etag {
                request = request.header("if-none-match", etag);
            }
        }
        let response = match request.send().await {
            Ok(response) => response,
            // Network failure: silent degradation in the background path —
            // the cached overlay and its validator stay valid; only the
            // freshness window moves. The strict `update --models` path
            // surfaces the transport error into the aggregate (upstream
            // models.ts `refresh` collects it into `errors`).
            Err(error) => {
                if strict {
                    return Err(AiError::Http(error.to_string()));
                }
                tracing::warn!(
                    provider = %self.config.provider_id,
                    error = %error,
                    "remote catalog fetch failed; keeping cached overlay"
                );
                self.persist_entry(|state| ModelsStoreEntry {
                    models: state.remote_values.clone(),
                    etag: state.etag.clone(),
                    last_modified: state.last_modified,
                    ..ModelsStoreEntry::default()
                });
                return Ok(changed);
            }
        };
        let status = response.status().as_u16();

        if status == 304 {
            // Unchanged: the overlay (already restored) stays; only the
            // freshness window moves.
            self.persist_entry(|state| ModelsStoreEntry {
                models: state.remote_values.clone(),
                etag: state.etag.clone(),
                last_modified: state.last_modified,
                ..ModelsStoreEntry::default()
            });
            return Ok(changed);
        }
        if status == 404 || status == 501 {
            // A personalized catalog keeps the built-in models: 404/501 only
            // means pi.dev serves no catalog for this provider, and the
            // per-key transform still runs (upstream `withPersonalizedCatalog`).
            if self.personalize.is_some() {
                return self.publish_personalized(Vec::new()).await;
            }
            // The remote catalog no longer serves this provider: drop the
            // overlay (and its validator) so the baseline catalog shows.
            {
                let mut state = self
                    .state
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.etag = None;
                state.last_modified = Some(0);
                state.remote_models.clear();
                state.remote_values.clear();
            }
            // Lock ordering is always state -> registry.dynamic (see
            // `apply_registration`), so a concurrent refresh cannot deadlock.
            self.registry
                .dynamic
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.config.provider_id);
            self.persist_entry(|_| ModelsStoreEntry {
                last_modified: Some(0),
                ..ModelsStoreEntry::default()
            });
            return Ok(true);
        }
        if !(200..300).contains(&status) {
            // Transient failure: the cached body and its validator stay valid,
            // so keep the etag and let the next refresh revalidate instead of
            // re-downloading (upstream behavior). The strict path still moves
            // the freshness window and then reports the upstream failure text.
            self.persist_entry(|state| ModelsStoreEntry {
                models: state.remote_values.clone(),
                etag: state.etag.clone(),
                last_modified: state.last_modified,
                ..ModelsStoreEntry::default()
            });
            if strict {
                return Err(AiError::NonRetryable(format!(
                    "Model catalog request failed for {}: {status}",
                    self.config.provider_id
                )));
            }
            tracing::warn!(
                provider = %self.config.provider_id,
                status,
                "remote catalog refresh got an error status; keeping cached overlay"
            );
            return Ok(changed);
        }

        if let Some(length) = response.content_length() {
            if length as usize > REMOTE_CATALOG_MAX_BODY_BYTES {
                return Err(AiError::ResponseTooLarge {
                    limit: REMOTE_CATALOG_MAX_BODY_BYTES,
                });
            }
        }
        // Read the validators before `text()` consumes the response.
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        // Upstream `readCatalog`: `Date.parse(last-modified)`, where a missing
        // or invalid header is 0. `Date.parse` yields Unix milliseconds.
        let last_modified = response
            .headers()
            .get("last-modified")
            .and_then(|value| value.to_str().ok())
            .and_then(crate::ai::transport::parse_http_date)
            .map(|seconds| seconds.saturating_mul(1000))
            .unwrap_or(0);
        let body = response
            .text()
            .await
            .map_err(|error| AiError::Http(error.to_string()))?;
        if body.len() > REMOTE_CATALOG_MAX_BODY_BYTES {
            return Err(AiError::ResponseTooLarge {
                limit: REMOTE_CATALOG_MAX_BODY_BYTES,
            });
        }
        let values = parse_remote_catalog(&body)?;
        let models = self.runtime_models(&values);
        // Upstream `isNewer(catalog.lastModified, localGeneratedAt)`: only a
        // remote catalog generated after the built-in snapshot may replace it.
        // A stale (or timestamp-less) catalog is persisted for the next
        // revalidation but never registered, so the baseline shows through.
        let remote_is_newer = remote_catalog_is_newer(Some(last_modified), self.local_generated_at);
        if self.personalize.is_some() {
            // The personalized list replaces the catalog, so the raw pi.dev
            // overlay is never stored (upstream `withPersonalizedCatalog`).
            // The gate only removes the stale pi.dev catalog from the merge;
            // the built-in models always participate.
            let remote = if remote_is_newer { models } else { Vec::new() };
            return self.publish_personalized(remote).await;
        }
        let applied = if remote_is_newer { models } else { Vec::new() };
        let changed = {
            let mut state = self
                .state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let changed = state.remote_models != applied;
            state.etag = etag;
            state.last_modified = Some(last_modified);
            state.remote_models = applied;
            state.remote_values = values;
            changed
        };
        self.persist_entry(|state| ModelsStoreEntry {
            models: state.remote_values.clone(),
            etag: state.etag.clone(),
            last_modified: state.last_modified,
            ..ModelsStoreEntry::default()
        });
        self.apply_registration();
        Ok(changed)
    }

    /// Merge the built-in and freshly fetched catalogs, run the personalized
    /// transform, store the result as the overlay — persisted verbatim, so it
    /// restores offline — and register it (upstream
    /// `withPersonalizedCatalog`'s refresh). A failed transform propagates
    /// without touching the previous state.
    async fn publish_personalized(&self, remote: Vec<Model>) -> AiResult<bool> {
        let Some(personalize) = self.personalize.clone() else {
            return Ok(false);
        };
        let merged = merge_models(&self.baseline, remote);
        let (personalized, values) = personalize.as_ref()(merged).await?;
        {
            let mut state = self
                .state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.etag = None;
            state.remote_models = personalized;
            state.remote_values = values;
        }
        self.persist_entry(|state| ModelsStoreEntry {
            models: state.remote_values.clone(),
            ..ModelsStoreEntry::default()
        });
        self.apply_registration();
        Ok(true)
    }

    /// Restore the persisted overlay from `models-store.json` when this is
    /// the first pass after startup (upstream's offline/cache-only phase).
    fn restore_persisted_overlay(&self) -> bool {
        let should_restore = {
            let state = self
                .state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.etag.is_none() && state.remote_models.is_empty()
        };
        if !should_restore {
            return false;
        }
        let Some(entry) = read_models_store_entry(&self.config.agent_dir, &self.config.provider_id)
        else {
            return false;
        };
        if entry.etag.is_none() && entry.models.is_empty() {
            return false;
        }
        // Upstream `remoteModels(entry, localGeneratedAt)`: a stored catalog
        // that is not newer than the built-in model data is not *applied*, so
        // the baseline stays in effect. Its raw body and validators are still
        // loaded into the state (upstream keeps `context.stored`), so a failed
        // revalidation keeps the cached body at hand. Entries written before the
        // manifest timestamp existed carry no `lastModified` and are not
        // applied either. A personalized overlay restores its saved list
        // unconditionally: it holds the user's key-specific list, not a pi.dev
        // catalog (upstream `withPersonalizedCatalog` never gates the restored
        // list).
        let applies = self.personalize.is_some()
            || remote_catalog_is_newer(entry.last_modified, self.local_generated_at);
        let models: Vec<Model> = entry
            .models
            .iter()
            .filter_map(|value| serde_json::from_value::<RemoteCatalogModel>(value.clone()).ok())
            .filter_map(|entry| self.runtime_model(&entry))
            .collect();
        {
            let mut state = self
                .state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Another refresh may have populated the state concurrently.
            if state.etag.is_some() || !state.remote_models.is_empty() {
                return false;
            }
            state.etag = entry.etag;
            state.last_modified = entry.last_modified;
            state.remote_values = entry.models.clone();
            state.remote_models = if applies { models } else { Vec::new() };
        }
        self.apply_registration();
        applies
    }

    /// Reconcile the registry with the current state: register (or replace)
    /// the overlay provider when the merged list is non-empty, unregister
    /// otherwise. The registry's dynamic map wins over statically registered
    /// providers, so readers switch atomically on the next lookup. Lock
    /// ordering is state -> registry.dynamic everywhere.
    fn apply_registration(&self) {
        let merged = self.merged_models();
        if merged.is_empty() {
            self.registry
                .dynamic
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.config.provider_id);
            return;
        }
        let provider = match super::construction::CatalogProviderEntry::from_models(
            &self.config.provider_id,
            &self.display_name,
            &merged,
        ) {
            Ok(provider) => provider,
            Err(error) => {
                tracing::warn!(
                    provider = %self.config.provider_id,
                    %error,
                    "remote catalog overlay could not be registered; keeping baseline"
                );
                return;
            }
        };
        self.registry
            .dynamic
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.config.provider_id.clone(), provider);
    }

    /// Baseline + remote models, remote winning per id (upstream
    /// `mergeModels`). A personalized overlay replaces the provider's model
    /// list (upstream `withPersonalizedCatalog`'s `getAllModels`), falling
    /// back to the built-in catalog while nothing was restored or refreshed.
    fn merged_models(&self) -> Vec<Model> {
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.personalize.is_some() {
            return if state.remote_models.is_empty() {
                self.baseline.clone()
            } else {
                state.remote_models.clone()
            };
        }
        merge_models(&self.baseline, state.remote_models.clone())
    }

    /// Convert raw catalog JSON values into runtime models, skipping entries
    /// without a usable id/api (typed warn, no failure — 宁缺毋滥).
    fn runtime_models(&self, values: &[serde_json::Value]) -> Vec<Model> {
        values
            .iter()
            .filter_map(
                |value| match serde_json::from_value::<RemoteCatalogModel>(value.clone()) {
                    Ok(entry) => Some(entry),
                    Err(error) => {
                        tracing::warn!(
                            provider = %self.config.provider_id,
                            %error,
                            "skipping unparsable remote catalog entry"
                        );
                        None
                    }
                },
            )
            .filter_map(|entry| self.runtime_model(&entry))
            .collect()
    }

    /// Convert one remote catalog entry into a runtime [`Model`], applying
    /// the same defaults as the embedded catalog loader (4096 context /
    /// maxTokens, `["text"]` input).
    fn runtime_model(&self, entry: &RemoteCatalogModel) -> Option<Model> {
        if entry.id.is_empty() {
            return None;
        }
        if entry
            .r#type
            .as_deref()
            .is_some_and(|r#type| r#type != "chat")
        {
            return None;
        }
        let Some(api) = entry.api.clone() else {
            tracing::warn!(
                provider = %self.config.provider_id,
                model = %entry.id,
                "skipping remote catalog entry without an api id"
            );
            return None;
        };
        let base_url = entry.base_url.clone().unwrap_or_default();
        let cost = entry.cost.clone().unwrap_or_default();
        Some(Model {
            id: entry.id.clone(),
            name: entry.name.clone().unwrap_or_else(|| entry.id.clone()),
            api,
            provider: self.config.provider_id.clone(),
            base_url,
            reasoning: entry.reasoning,
            thinking_level_map: entry.thinking_level_map.clone(),
            input: entry
                .input
                .clone()
                .unwrap_or_else(|| vec!["text".to_string()]),
            cost: ModelCost {
                rates: ModelCostRates {
                    input: cost.input,
                    output: cost.output,
                    cache_read: cost.cache_read,
                    cache_write: cost.cache_write,
                },
                tiers: cost
                    .tiers
                    .into_iter()
                    .map(|tier| ModelCostTier {
                        rates: ModelCostRates {
                            input: tier.input,
                            output: tier.output,
                            cache_read: tier.cache_read,
                            cache_write: tier.cache_write,
                        },
                        input_tokens_above: tier.input_tokens_above,
                    })
                    .collect(),
            },
            context_window: entry.context_window.unwrap_or(4096),
            max_tokens: entry.max_tokens.unwrap_or(4096),
            sampling_params: None,
            headers: entry.headers.clone(),
            thinking_token_budget_field: None,
            input_limits: None,
            prompt_cache: None,
            sampling_params_by_thinking_level: None,
            model_type: Some(ModelKind::Chat),
            compat: entry.compat.clone(),
        })
    }

    /// Persist the overlay under a file lock (upstream `ModelsStore.write`).
    /// `compose` sees the current state snapshot; persistence failures are
    /// swallowed (the in-memory overlay remains authoritative for the run).
    fn persist_entry(&self, compose: impl FnOnce(&OverlayState) -> ModelsStoreEntry) {
        let entry = {
            let state = self
                .state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut entry = compose(&state);
            // `checkedAt` is Unix milliseconds, matching upstream's
            // `Date.now()` and the radius/JS models-store writers.
            entry.checked_at = Some(crate::ai::util::now_ms());
            entry
        };
        let _ = write_models_store_entry(&self.config.agent_dir, &self.config.provider_id, entry);
    }
}

/// Delay before the background overlay's first network revalidation, derived
/// from the persisted `checkedAt` (the upstream
/// `remote-catalog-provider.ts` freshness window). Both timestamps are Unix
/// **milliseconds**, matching `Date.now()` and the other models-store writers
/// (`radius.rs`, the JS `publish({persist})` path).
///
/// A stored check younger than `interval` is still fresh: the task sleeps the
/// remainder of the window instead of restarting with a full-interval sleep
/// (the old behavior delayed every restart by 4h even right after a check). An
/// already-expired timestamp yields `Duration::ZERO`, so the first hop runs
/// immediately. A timestamp in the future is treated as "just checked" and
/// clamps to one interval (a clock adjustment must not park the task for
/// longer than the configured cadence). `None` — no persisted check at all —
/// keeps the legacy one-interval delay, so a fresh install does not fan out
/// one request per provider at startup.
pub fn remote_catalog_initial_delay(
    interval: Duration,
    checked_at: Option<u64>,
    now_ms: u64,
) -> Duration {
    let Some(checked_at) = checked_at else {
        return interval;
    };
    let elapsed = Duration::from_millis(now_ms.saturating_sub(checked_at));
    interval.saturating_sub(elapsed)
}

/// Baseline + remote models, remote entries winning per id (upstream
/// `mergeModels`).
fn merge_models(baseline: &[Model], remote: Vec<Model>) -> Vec<Model> {
    let mut merged = baseline.to_vec();
    for model in remote {
        match merged.iter().position(|entry| entry.id == model.id) {
            Some(index) => merged[index] = model,
            None => merged.push(model),
        }
    }
    merged
}

/// Parse a remote catalog document. Accepts the same shapes as upstream
/// `parseCatalog`: a bare model array, a `{ "models": [...] }` envelope, or an
/// id-keyed object map.
fn parse_remote_catalog(body: &str) -> AiResult<Vec<serde_json::Value>> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(AiError::Json)?;
    let entries = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(object) => match object.get("models") {
            Some(serde_json::Value::Array(items)) => items.clone(),
            Some(serde_json::Value::Object(models)) => models.values().cloned().collect::<Vec<_>>(),
            _ => object.into_values().collect(),
        },
        _ => {
            return Err(AiError::NonRetryable(format!(
                "invalid remote catalog document: expected an array or object, got {value:?}"
            )));
        }
    };
    Ok(entries
        .into_iter()
        .filter(|entry| {
            entry
                .as_object()
                .is_some_and(|object| object.contains_key("id"))
        })
        .collect())
}

/// The overlay personalizer of one provider (upstream `model-runtime.ts`
/// wraps OpenRouter with `withPersonalizedCatalog`, `#10672`): the merged
/// catalog is filtered to the models the stored key may use, with the key's
/// context size, max output and prices, and the result is persisted as the
/// offline cache.
fn personalize_for(provider_id: &str) -> Option<Arc<PersonalizeCatalog>> {
    if provider_id != "openrouter" {
        return None;
    }
    Some(Arc::new(|catalog: Vec<Model>| {
        Box::pin(async move {
            let key = super::catalog::openrouter::openrouter_api_key();
            let base_url = super::catalog::openrouter::configured_base_url();
            let models = super::catalog::openrouter::personalize_openrouter_catalog(
                catalog,
                &base_url,
                key.as_deref(),
                super::catalog::openrouter::OPENROUTER_USER_MODELS_TIMEOUT,
            )
            .await?;
            // Persist the personalized list in the models-store wire shape so
            // it restores offline.
            let values = super::catalog::openrouter::model_wire_values(&models);
            Ok((models, values))
        })
            as std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = AiResult<(Vec<Model>, Vec<serde_json::Value>)>>
                        + Send,
                >,
            >
    }))
}

/// Install a [`RemoteCatalogOverlay`] for every built-in catalog provider and
/// spawn its background refresh task (upstream `model-runtime.ts` wraps each
/// built-in provider with `withRemoteCatalog`). The catalog URL follows the
/// upstream shape: `{base}/api/models/providers/{id}?types=chat,image,classifier`.
/// OpenRouter gets a personalized overlay (`withPersonalizedCatalog`).
///
/// Gated on `PI_OFFLINE` (upstream's offline gate): offline returns 0 and no
/// network state is touched. The base URL is overridable with
/// `PI_CATALOG_BASE_URL` (default [`REMOTE_CATALOG_DEFAULT_BASE_URL`]). Spawn
/// failures (no tokio runtime — [`RemoteCatalogOverlay::spawn_refresh_task`]
/// already guards on `Handle::try_current`) are skipped silently. Returns the
/// number of overlays successfully spawned.
pub fn install_remote_catalog_overlays(registry: &Arc<ProviderRegistry>) -> usize {
    if crate::config::env::is_offline() {
        return 0;
    }
    let base = std::env::var("PI_CATALOG_BASE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| REMOTE_CATALOG_DEFAULT_BASE_URL.to_string());
    let agent_dir = super::settings::agent_dir();
    let mut spawned = 0usize;
    for (provider_id, _) in super::catalog::CATALOG_FILES {
        let config = RemoteCatalogOverlayConfig {
            provider_id: (*provider_id).to_string(),
            catalog_url: format!(
                "{base}/api/models/providers/{provider_id}?types=chat,image,classifier"
            ),
            agent_dir: agent_dir.clone(),
            interval: None,
        };
        let overlay = match RemoteCatalogOverlay::new_with_provider_personalizer(
            config,
            Arc::clone(registry),
        ) {
            Ok(overlay) => Arc::new(overlay),
            Err(error) => {
                // Catalog load failure for one provider must not block the
                // rest of the fleet (typed log, no panic).
                tracing::warn!(
                    provider = %provider_id,
                    %error,
                    "remote catalog overlay install failed; skipping provider"
                );
                continue;
            }
        };
        if overlay.spawn_refresh_task() {
            spawned += 1;
        }
    }
    spawned
}
