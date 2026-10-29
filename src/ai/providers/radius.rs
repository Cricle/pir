//! Radius gateway provider with a persisted, dynamically refreshed catalog
//! (upstream `providers/radius.ts` + `providers/radius-config.ts`).
//!
//! The shipped `radius.json` snapshot is only the baseline: a gateway catalog
//! for the account (Radius org owners can disable models) replaces it once
//! known. Restoring a persisted catalog and fetching `/v1/config` run through
//! the two-phase [`Provider::refresh_models`] contract, so `ProviderRegistry::
//! refresh` owns the generation checks and persistence.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde::Deserialize;

use super::catalog::BuiltinProvider;
use super::construction::CatalogProviderEntry;
use super::models_store::ModelsStoreEntry;
use crate::ai::auth::{ApiKeyAuth, StoredCredential};
use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::provider::{
    ModelsPublication, Provider, ProviderBox, ProviderFuture, RefreshModelsContext,
};
use crate::ai::types::{
    AbortSignal, Context, Model, ModelCost, ModelKind, ProviderStreamHooks, StreamOptions,
};

/// Upstream `normalizeRadiusGatewayUrl`: add a scheme when missing and drop
/// trailing slashes.
pub fn normalize_gateway_url(value: &str) -> String {
    let with_scheme = if value.starts_with("http://") || value.starts_with("https://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// A model entry from the gateway config (upstream `RadiusGatewayModel`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RadiusGatewayModel {
    id: String,
    name: String,
    reasoning: bool,
    #[serde(default)]
    thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    input: Vec<String>,
    cost: ModelCost,
    context_window: u64,
    max_tokens: u64,
}

/// The gateway `/v1/config` payload (upstream `RadiusGatewayConfig`).
#[derive(Debug, Clone)]
struct RadiusGatewayConfig {
    base_url: String,
    models: Vec<RadiusGatewayModel>,
}

impl RadiusGatewayConfig {
    /// Parse and sanitize the payload, dropping malformed model entries
    /// (upstream `sanitizeRadiusGatewayConfig`).
    fn parse(value: &serde_json::Value) -> Option<Self> {
        let object = value.as_object()?;
        let base_url = object.get("baseUrl")?.as_str()?.to_string();
        let models = object
            .get("models")?
            .as_array()?
            .iter()
            .filter_map(|entry| serde_json::from_value::<RadiusGatewayModel>(entry.clone()).ok())
            .filter(|model| !model.id.is_empty() && !model.name.is_empty())
            .collect::<Vec<_>>();
        Some(Self { base_url, models })
    }

    /// Runtime models for `provider_id` (upstream `getRadiusModelsFromConfig`).
    fn into_models(self, provider_id: &str) -> Vec<Model> {
        let base_url = self.base_url;
        self.models
            .into_iter()
            .map(|model| Model {
                id: model.id,
                name: model.name,
                api: "pi-messages".to_string(),
                provider: provider_id.to_string(),
                base_url: base_url.clone(),
                reasoning: model.reasoning,
                thinking_level_map: model.thinking_level_map,
                input: model.input,
                cost: model.cost,
                context_window: model.context_window,
                max_tokens: model.max_tokens,
                sampling_params: None,
                headers: None,
                thinking_token_budget_field: None,
                input_limits: None,
                prompt_cache: None,
                sampling_params_by_thinking_level: None,
                model_type: Some(ModelKind::Chat),
                compat: None,
            })
            .collect()
    }
}

/// Decode persisted models (`models-store.json` `Model` wire values), keeping
/// only this provider's entries (upstream restores `model.provider === id`).
fn decode_stored_models(values: &[serde_json::Value], provider_id: &str) -> Vec<Model> {
    values
        .iter()
        .filter_map(|value| serde_json::from_value::<Model>(value.clone()).ok())
        .filter(|model| model.provider == provider_id)
        .collect()
}

/// Legacy Radius catalogs cached in the OAuth credential's `gatewayConfig`
/// extra field, imported on first refresh (upstream pre-ModelsStore path).
fn legacy_credential_models(
    credential: Option<&StoredCredential>,
    provider_id: &str,
) -> Vec<Model> {
    let Some(StoredCredential::OAuth(credential)) = credential else {
        return Vec::new();
    };
    credential
        .extra
        .get("gatewayConfig")
        .and_then(RadiusGatewayConfig::parse)
        .map(|config| config.into_models(provider_id))
        .unwrap_or_default()
}

/// Fetch the gateway config (upstream `loadRadiusGatewayConfig`).
async fn fetch_gateway_config(
    gateway: &str,
    api_key: Option<&str>,
    signal: &AbortSignal,
) -> AiResult<RadiusGatewayConfig> {
    let client = crate::ai::transport::ProviderRequestClient::with_default_config()?;
    let mut headers = BTreeMap::new();
    headers.insert("accept".to_string(), "application/json".to_string());
    if let Some(api_key) = api_key.filter(|key| !key.is_empty()) {
        headers.insert("authorization".to_string(), format!("Bearer {api_key}"));
    }
    let url = format!("{}/v1/config", gateway.trim_end_matches('/'));
    let mut body = Vec::new();
    client
        .request_streaming(
            reqwest::Method::GET,
            &url,
            &headers,
            Vec::new(),
            Some(signal),
            &ProviderStreamHooks::default(),
            &mut |chunk| {
                body.extend_from_slice(chunk);
                Ok(())
            },
        )
        .await?;
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|error| {
        AiError::NonRetryable(format!("Invalid Radius config from {gateway}: {error}"))
    })?;
    RadiusGatewayConfig::parse(&value)
        .filter(|config| !config.models.is_empty())
        .ok_or_else(|| AiError::NonRetryable(format!("Invalid Radius config from {gateway}")))
}

/// Mutable catalog + dispatch state, swapped as one unit by a successful
/// refresh.
struct RadiusState {
    models: Vec<Model>,
    dispatch: ProviderBox,
}

/// Shared state behind an `Arc` so [`Provider::refresh_models`] can move it
/// into its future and install updates from a sync publication callback.
struct RadiusCore {
    provider_id: String,
    display_name: String,
    gateway: String,
    state: RwLock<RadiusState>,
}

impl RadiusCore {
    /// Install a new catalog (and the adapter built for it) when it is
    /// buildable; an invalid list leaves the previous state untouched.
    fn install_models(&self, models: Vec<Model>) {
        if models.is_empty() {
            return;
        }
        let Ok(dispatch) =
            CatalogProviderEntry::from_models(&self.provider_id, &self.display_name, &models)
        else {
            return;
        };
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.models = models;
        state.dispatch = dispatch;
    }

    fn models(&self) -> Vec<Model> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .models
            .clone()
    }

    fn dispatch(&self) -> ProviderBox {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .dispatch
            .clone()
    }

    /// One refresh phase (upstream `refreshModels`): restore the persisted
    /// catalog, import a legacy credential catalog, then (network phase only)
    /// fetch `/v1/config`.
    async fn refresh(core: &Arc<RadiusCore>, context: RefreshModelsContext) -> AiResult<()> {
        if let Some(stored) = &context.stored {
            let restored = decode_stored_models(&stored.models, &core.provider_id);
            if !restored.is_empty() {
                let update = models_update(Arc::clone(core), restored);
                if !context
                    .publish(ModelsPublication {
                        persist: None,
                        update: Some(update),
                    })
                    .await
                {
                    return Ok(());
                }
            }
        }
        if context.stored.is_none() {
            let legacy = legacy_credential_models(context.credential.as_ref(), &core.provider_id);
            if !legacy.is_empty() {
                let values = legacy
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<Result<Vec<_>, _>>()?;
                let update = models_update(Arc::clone(core), legacy);
                if !context
                    .publish(ModelsPublication {
                        persist: Some(Some(ModelsStoreEntry {
                            models: values,
                            last_modified: None,
                            checked_at: Some(crate::ai::util::now_ms()),
                            etag: None,
                        })),
                        update: Some(update),
                    })
                    .await
                {
                    return Ok(());
                }
            }
        }

        if !context.allow_network || context.signal.is_aborted() {
            return Ok(());
        }
        let api_key = match context.credential.as_ref() {
            Some(StoredCredential::OAuth(credential)) => Some(credential.access.clone()),
            Some(StoredCredential::ApiKey(credential)) => credential.key.clone(),
            None => None,
        };
        let config =
            fetch_gateway_config(&core.gateway, api_key.as_deref(), &context.signal).await?;
        if context.signal.is_aborted() {
            return Ok(());
        }
        let models = config.into_models(&core.provider_id);
        let values = models
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?;
        let update = models_update(Arc::clone(core), models);
        context
            .publish(ModelsPublication {
                persist: Some(Some(ModelsStoreEntry {
                    models: values,
                    last_modified: None,
                    checked_at: Some(crate::ai::util::now_ms()),
                    etag: None,
                })),
                update: Some(update),
            })
            .await;
        Ok(())
    }
}

/// A publication update closure: installs the new catalog into `core`. The
/// publisher runs it only while the refresh generation is still current, so a
/// superseded refresh never installs its list.
fn models_update(core: Arc<RadiusCore>, models: Vec<Model>) -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(move || core.install_models(models.clone()))
}

/// A Radius gateway provider with a dynamically refreshed catalog.
pub struct RadiusProvider {
    core: Arc<RadiusCore>,
}

impl RadiusProvider {
    /// Build the provider from its embedded catalog entry (baseline models).
    pub fn from_catalog(entry: &BuiltinProvider) -> AiResult<ProviderBox> {
        Self::new(&entry.id, &entry.display_name, entry.models.clone(), None)
    }

    /// Build the provider for a provider id, display name, baseline model list
    /// and optional gateway override (upstream `radiusProvider(options)`).
    // `new` intentionally returns the type-erased `ProviderBox`, matching the
    // upstream factory helper; callers register the trait object, not `Self`.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        provider_id: &str,
        display_name: &str,
        baseline: Vec<Model>,
        gateway: Option<&str>,
    ) -> AiResult<ProviderBox> {
        let dispatch = CatalogProviderEntry::from_models(provider_id, display_name, &baseline)?;
        Ok(Arc::new(Self {
            core: Arc::new(RadiusCore {
                provider_id: provider_id.to_string(),
                display_name: display_name.to_string(),
                gateway: crate::ai::auth::oauth::radius::resolve_radius_gateway(gateway),
                state: RwLock::new(RadiusState {
                    models: baseline,
                    dispatch,
                }),
            }),
        }))
    }
}

impl Provider for RadiusProvider {
    fn id(&self) -> &str {
        &self.core.provider_id
    }

    fn name(&self) -> &str {
        &self.core.display_name
    }

    fn get_models(&self) -> Vec<Model> {
        self.core.models()
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new(
            "Radius API key",
            vec!["RADIUS_API_KEY".to_string()],
        )]
    }

    fn oauth_flow(&self) -> Option<crate::ai::auth::OAuthFlowBox> {
        crate::ai::auth::oauth_flow_for("radius", Some(&self.core.gateway))
    }

    fn supports_refresh(&self) -> bool {
        true
    }

    fn refresh_models(
        &self,
        context: RefreshModelsContext,
    ) -> Option<ProviderFuture<AiResult<()>>> {
        let core = Arc::clone(&self.core);
        Some(Box::pin(async move {
            RadiusCore::refresh(&core, context).await
        }))
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.core.dispatch().stream(model, context, options)
    }

    fn stream_shared(
        &self,
        model: &Model,
        context: Arc<Context>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.core.dispatch().stream_shared(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: Arc<Context>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let options = crate::ai::provider::simple_stream_options(model, options);
        self.core.dispatch().stream_shared(model, context, options)
    }
}
