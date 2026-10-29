//! Virtual models: catalog entries that route each request to a physical
//! model.
//!
//! Rust equivalent of upstream `packages/coding-agent/src/core/virtual-models.ts`
//! (the registration/catalog half; routing itself lives in the session's
//! `prepare_request` projection, mirroring upstream `_installAgentRequestProjection`).
//!
//! The selection (`setModel`, the session model, `ctx.model`) may name a
//! virtual model. Everything below the routing step only sees physical models:
//! providers stream them. A virtual model never reaches a provider directly.
//!
//! pir's first slice uses static routing: a virtual model registration names a
//! physical model (`provider`/`id`) every request routes to. Upstream's
//! per-request `route(request)` callback is a superset; it is implemented as
//! an optional per-registration callback ([`VirtualRouteFn`], consulted by
//! [`VirtualModelRegistry::route_callback`]) whose failures fall back to the
//! static route below.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde_json::Value as JsonValue;

use crate::ai::types::{Model, ModelKind};

/// API id of virtual catalog entries. Requests for it fail unless routed
/// first (upstream `VIRTUAL_MODEL_API`).
pub const VIRTUAL_MODEL_API: &str = "pi-virtual";

/// Custom entry type that stores a virtual-model registration on the session
/// branch so it survives resume (upstream `VIRTUAL_MODEL_STATE_ENTRY`; upstream
/// stores router state, pir stores the static registration itself).
pub const VIRTUAL_MODEL_STATE_ENTRY: &str = "pi.virtual-model-state";

/// Whether a model names a virtual model (upstream `isVirtualModel`).
pub fn is_virtual_model(model: &Model) -> bool {
    model.api == VIRTUAL_MODEL_API
}

/// The physical model a virtual model routes to (upstream
/// `VirtualModelDefinition` reduced to pir's static route).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PhysicalModelRef {
    /// Provider the physical model belongs to.
    pub provider: String,
    /// Physical model id.
    pub id: String,
}

/// Per-request limits a virtual model overrides (upstream
/// `VirtualModelDefinition.contextWindow`/`maxTokens`: limits shown before the
/// first response; compaction thresholds read them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VirtualModelOverride {
    /// Context window used for the compaction threshold while the virtual
    /// model is selected.
    pub context_window: Option<u64>,
    /// Output token cap carried on the catalog entry.
    pub max_tokens: Option<u64>,
}

/// One registered virtual model (upstream `VirtualModelDefinition`, static
/// routing form).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualModelRegistration {
    /// Model id. Must not be the id of a physical model of `provider`.
    pub id: String,
    /// Provider the virtual model is listed under. May be a provider with
    /// physical models.
    pub provider: String,
    /// Display name; defaults to the id.
    #[serde(default)]
    pub name: Option<String>,
    /// The physical model every request routes to.
    pub physical: PhysicalModelRef,
    /// Whether the model reports reasoning support (defaults to the physical
    /// model's flag at routing time when unset).
    #[serde(default)]
    pub reasoning: Option<bool>,
    /// Extra system prompt prepended for requests routed under this model.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Per-request limit overrides (compaction thresholds read them).
    #[serde(default)]
    pub model_override: Option<VirtualModelOverride>,
    /// Who registered the model: `"session"` (session storage restore),
    /// `"plugin:<path>"` (a plugin/extension registration), or `"host"`
    /// (embedder). Registry sync uses the `plugin:` prefix to drop
    /// registrations whose plugin went away.
    #[serde(default)]
    pub source: String,
}

impl VirtualModelRegistration {
    /// Validate a wire JSON definition (`registerVirtualModel` payload) into a
    /// registration tagged with `source`. `provider` defaults to the physical
    /// model's provider when omitted.
    pub fn from_wire(value: &JsonValue, source: &str) -> Result<Self, String> {
        let id = value
            .get("id")
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if id.is_empty() {
            return Err("virtual model id must not be empty".to_owned());
        }
        let physical = value.get("physical").ok_or_else(|| {
            format!("virtual model {id}: `physical: {{provider, id}}` is required")
        })?;
        let physical_provider = physical
            .get("provider")
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let physical_id = physical
            .get("id")
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if physical_provider.is_empty() || physical_id.is_empty() {
            return Err(format!(
                "virtual model {id}: physical provider and id must not be empty"
            ));
        }
        let provider = value
            .get("provider")
            .and_then(JsonValue::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| physical_provider.to_owned());
        let model_override =
            value
                .get("modelOverride")
                .map(|override_value| VirtualModelOverride {
                    context_window: override_value
                        .get("contextWindow")
                        .and_then(JsonValue::as_u64),
                    max_tokens: override_value.get("maxTokens").and_then(JsonValue::as_u64),
                });
        Ok(Self {
            id: id.to_owned(),
            provider,
            name: value
                .get("name")
                .and_then(JsonValue::as_str)
                .map(str::to_owned),
            physical: PhysicalModelRef {
                provider: physical_provider.to_owned(),
                id: physical_id.to_owned(),
            },
            reasoning: value.get("reasoning").and_then(JsonValue::as_bool),
            system_prompt: value
                .get("systemPrompt")
                .and_then(JsonValue::as_str)
                .map(str::to_owned),
            model_override,
            source: source.to_owned(),
        })
    }

    /// The display name (upstream `createVirtualModel`: the id when unnamed).
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// The catalog entry key (`provider/id`).
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// Build the catalog entry [`Model`] for this virtual model (upstream
    /// `createVirtualModel`): a keyless, cost-free entry whose api is
    /// [`VIRTUAL_MODEL_API`]. `physical` (when resolved) donates its wire api,
    /// base url, input list, and reasoning flag for the fields a catalog
    /// consumer needs before routing.
    pub fn catalog_model(&self, physical: Option<&Model>) -> Model {
        let context_window = self
            .model_override
            .and_then(|override_value| override_value.context_window)
            .unwrap_or_else(|| physical.map(|model| model.context_window).unwrap_or(0));
        let max_tokens = self
            .model_override
            .and_then(|override_value| override_value.max_tokens)
            .unwrap_or_else(|| physical.map(|model| model.max_tokens).unwrap_or(0));
        let reasoning = self
            .reasoning
            .unwrap_or_else(|| physical.map(|model| model.reasoning).unwrap_or(false));
        Model {
            id: self.id.clone(),
            name: self.display_name().to_owned(),
            api: VIRTUAL_MODEL_API.to_owned(),
            provider: self.provider.clone(),
            base_url: String::new(),
            reasoning,
            thinking_level_map: None,
            input: physical
                .map(|model| model.input.clone())
                .unwrap_or_else(|| vec!["text".to_owned(), "image".to_owned()]),
            cost: crate::ai::types::ModelCost {
                rates: crate::ai::types::ModelCostRates {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                tiers: Vec::new(),
            },
            context_window,
            max_tokens,
            sampling_params: None,
            headers: None,
            thinking_token_budget_field: None,
            input_limits: None,
            prompt_cache: None,
            sampling_params_by_thinking_level: None,
            model_type: Some(ModelKind::Chat),
            compat: None,
        }
    }
}

/// Registry of virtual models (upstream `ModelRuntime.virtualModels`).
///
/// An additive overlay on the provider registry: registrations are keyed by
/// `(provider, id)` and re-registering the same pair replaces the previous
/// one. The overlay never hides physical models — routing resolves through the
/// physical catalog, and [`VirtualModelRegistry::conflicts_with_physical`] is
/// checked by the caller that owns the physical catalog.
#[derive(Default)]
pub struct VirtualModelRegistry {
    inner: RwLock<BTreeMap<(String, String), VirtualModelRegistration>>,
    /// Per-registration `route(request)` callbacks (upstream
    /// `VirtualModelDefinition.route`), installed by the extension runtime's
    /// sync. A missing callback (or a failing one) falls back to the static
    /// `physical` route.
    route_callbacks: RwLock<BTreeMap<(String, String), VirtualRouteFn>>,
}

impl VirtualModelRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (upsert) a virtual model. Returns `Err` when `id` is empty or
    /// the registration is malformed (validated by
    /// [`VirtualModelRegistration::from_wire`] for wire input).
    pub fn register(&self, registration: VirtualModelRegistration) -> Result<(), String> {
        if registration.id.trim().is_empty() || registration.provider.trim().is_empty() {
            return Err("virtual model provider and id must not be empty".to_owned());
        }
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                (registration.provider.clone(), registration.id.clone()),
                registration,
            );
        Ok(())
    }

    /// Remove a virtual model. Returns whether one was registered.
    pub fn unregister(&self, provider: &str, id: &str) -> bool {
        self.route_callbacks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(provider.to_owned(), id.to_owned()));
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(provider.to_owned(), id.to_owned()))
            .is_some()
    }

    /// Look up a virtual model registration.
    pub fn get(&self, provider: &str, id: &str) -> Option<VirtualModelRegistration> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(provider.to_owned(), id.to_owned()))
            .cloned()
    }

    /// Every registration, in `(provider, id)` order.
    pub fn all(&self) -> Vec<VirtualModelRegistration> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// Whether a virtual model with this `provider`/`id` is registered.
    pub fn is_registered(&self, provider: &str, id: &str) -> bool {
        self.get(provider, id).is_some()
    }

    /// Install or drop the per-request route callback for one registration
    /// (upstream `VirtualModelDefinition.route`; `None` clears it). The
    /// callback is keyed like the registration and removed by
    /// [`Self::unregister`].
    pub fn set_route_callback(&self, provider: &str, id: &str, callback: Option<VirtualRouteFn>) {
        let mut callbacks = self
            .route_callbacks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match callback {
            Some(callback) => {
                callbacks.insert((provider.to_owned(), id.to_owned()), callback);
            }
            None => {
                callbacks.remove(&(provider.to_owned(), id.to_owned()));
            }
        }
    }

    /// The per-request route callback for one registration, when installed.
    pub fn route_callback(&self, provider: &str, id: &str) -> Option<VirtualRouteFn> {
        self.route_callbacks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(provider.to_owned(), id.to_owned()))
            .cloned()
    }
}

impl Clone for VirtualModelRegistry {
    fn clone(&self) -> Self {
        Self {
            inner: RwLock::new(
                self.inner
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            ),
            route_callbacks: RwLock::new(
                self.route_callbacks
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            ),
        }
    }
}

impl std::fmt::Debug for VirtualModelRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_map()
            .entries(
                self.all()
                    .iter()
                    .map(|registration| (registration.key(), registration.source.as_str())),
            )
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Per-request route callbacks (upstream `VirtualModelDefinition.route`)
// ---------------------------------------------------------------------------

/// The per-request routing input (upstream `ModelRouteRequest`, pir's JSON-wire
/// subset): the virtual catalog model, the selected thinking level, and the
/// conversation for this request. Serialized to the JS callback.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VirtualRouteRequest {
    /// The virtual catalog entry the request was selected under.
    pub model: JsonValue,
    /// Selected thinking level (upstream `ModelRouteRequest.thinkingLevel`).
    pub thinking_level: Option<String>,
    /// Conversation for this request, including system messages.
    pub messages: JsonValue,
}

/// The routing decision (upstream `ModelRoute`): the physical model that
/// handles the request. `thinkingLevel` is carried for a later slice — the
/// pir request projection (`AgentSession`'s `prepare_request`) currently
/// keeps the requested level and does not apply the decision's (documented
/// carry-through, not applied). `state` is the sticky router state: the JS
/// bridge (`crate::ai::providers::js_stream::js_virtual_route_callback`)
/// stores it per registration and feeds it back as `request.state` on the
/// next route (upstream stores it on the session branch under
/// [`VIRTUAL_MODEL_STATE_ENTRY`]; pir's surrogate is in-memory per installed
/// callback).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualRouteDecision {
    /// Physical model the request routes to (must exist and not be virtual).
    pub physical: PhysicalModelRef,
    /// Advisory thinking level from the callback.
    #[serde(default)]
    pub thinking_level: Option<String>,
    /// Sticky router state (upstream `ModelRoute.state`): `undefined` keeps
    /// the current state; a returned value is stored and fed back as
    /// `request.state` on the next route. Must be JSON-serializable.
    #[serde(default)]
    pub state: Option<JsonValue>,
}

/// One route-callback invocation: a request in, a decision (or failure) out.
pub type VirtualRouteFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<VirtualRouteDecision, String>> + Send>,
>;

/// The per-request route callback type (upstream `route(request)`).
///
/// Implementations (e.g. the JS bridge in
/// `crate::ai::providers::js_stream`) may await; failures fall back to the
/// registration's static `physical` route.
pub type VirtualRouteFn = Arc<dyn Fn(VirtualRouteRequest) -> VirtualRouteFuture + Send + Sync>;
