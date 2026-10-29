//! Provider construction: pick the protocol adapter for a merged model list.
//!
//! Each provider family's construction arm rides the same `provider-…`
//! feature as its adapter module (`src/ai/api/**`). With a family compiled
//! out, constructing a provider for its api fails with
//! [`catalog::missing_adapter_error`](super::catalog::missing_adapter_error)
//! naming the disabled feature — never a fallback to another adapter.

use std::collections::BTreeMap;
use std::sync::Arc;

#[cfg(feature = "provider-anthropic")]
use crate::ai::api::anthropic::AnthropicMessagesProvider;
#[cfg(feature = "provider-bedrock")]
use crate::ai::api::bedrock::BedrockConverseStreamProvider;
#[cfg(feature = "provider-google")]
use crate::ai::api::google::GoogleGenerativeAiProvider;
#[cfg(feature = "provider-google")]
use crate::ai::api::google_vertex::GoogleVertexProvider;
#[cfg(feature = "provider-mistral")]
use crate::ai::api::mistral::MistralConversationsProvider;
#[cfg(feature = "provider-openai")]
use crate::ai::api::openai::OpenAiCompatibleProvider;
#[cfg(feature = "provider-openai")]
use crate::ai::api::openai_responses::{
    AzureOpenAiResponsesProvider, OpenAiCodexResponsesProvider, OpenAiResponsesProvider,
};
use crate::ai::auth::{env_api_keys, ApiKeyAuth};
use crate::ai::error::{AiError, AiResult};
use crate::ai::provider::ProviderBox;
use crate::ai::types::{Model, ModelCost, ModelCostRates, ModelKind};

#[allow(clippy::too_many_arguments)]
pub fn model(
    provider: &str,
    id: &str,
    name: &str,
    api: &str,
    base_url: &str,
    context_window: u64,
    max_tokens: u64,
    input: Vec<&str>,
) -> Model {
    Model {
        id: id.to_string(),
        name: name.to_string(),
        api: api.to_string(),
        provider: provider.to_string(),
        base_url: base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: input.into_iter().map(str::to_string).collect(),
        cost: ModelCost {
            rates: ModelCostRates {
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

/// Build the runtime provider for a model list dispatch on the first model's
/// api id. Each family's arm is compiled only with its `provider-…` feature;
/// without it the arm returns the honest [`super::catalog::missing_adapter_error`]
/// (naming the feature) instead of quietly constructing a different adapter.
///
/// The google/google-vertex arms compare against the api-id string literals
/// rather than `api::google::GOOGLE_API_ID` /
/// `api::google_vertex::GOOGLE_VERTEX_API_ID`: those modules — and their
/// constants — are gated with `provider-google`. The literal values are
/// identical to the constants.
///
/// `display_name` is consumed by the google, openai-family and mistral arms
/// only; a build with just the other families sees it untouched.
#[cfg_attr(
    not(any(
        feature = "provider-google",
        feature = "provider-openai",
        feature = "provider-mistral"
    )),
    allow(unused_variables)
)]
pub(super) fn build_provider_from_models(
    provider_id: &str,
    display_name: &str,
    models: &[Model],
) -> AiResult<ProviderBox> {
    let first = models
        .first()
        .ok_or_else(|| AiError::UnknownProvider(provider_id.to_string()))?;
    let base_url = first.base_url.clone();
    if first.api == "anthropic-messages" {
        #[cfg(feature = "provider-anthropic")]
        {
            Ok(Arc::new(AnthropicMessagesProvider::new(
                provider_id,
                base_url,
                models.to_vec(),
            )?))
        }
        #[cfg(not(feature = "provider-anthropic"))]
        {
            Err(super::catalog::missing_adapter_error("anthropic-messages"))
        }
    } else if first.api == "pi-messages" {
        #[cfg(feature = "provider-openai")]
        {
            Ok(Arc::new(
                crate::ai::api::pi_messages::PiMessagesProvider::new(
                    provider_id,
                    base_url,
                    models.to_vec(),
                )?,
            ))
        }
        #[cfg(not(feature = "provider-openai"))]
        {
            Err(super::catalog::missing_adapter_error("pi-messages"))
        }
    } else if first.api == "google-generative-ai" {
        #[cfg(feature = "provider-google")]
        {
            let env_keys: Vec<String> = env_api_keys::all()
                .into_iter()
                .find(|(id, _)| *id == "google")
                .map(|(_, keys)| keys.iter().map(|key| (*key).to_string()).collect())
                .unwrap_or_else(|| {
                    vec!["GEMINI_API_KEY".to_string(), "GOOGLE_API_KEY".to_string()]
                });
            let provider = GoogleGenerativeAiProvider::new(provider_id, base_url, models.to_vec())?
                .with_display_name(display_name)
                .with_api_key_env(env_keys);
            Ok(Arc::new(provider))
        }
        #[cfg(not(feature = "provider-google"))]
        {
            Err(super::catalog::missing_adapter_error(
                "google-generative-ai",
            ))
        }
    } else if first.api == "google-vertex" {
        #[cfg(feature = "provider-google")]
        {
            let env_keys: Vec<String> = env_api_keys::all()
                .into_iter()
                .find(|(id, _)| *id == "google-vertex")
                .map(|(_, keys)| keys.iter().map(|key| (*key).to_string()).collect())
                .unwrap_or_else(|| {
                    vec![
                        "GOOGLE_CLOUD_API_KEY".to_string(),
                        "GOOGLE_API_KEY".to_string(),
                    ]
                });
            let provider = GoogleVertexProvider::new(provider_id, base_url, models.to_vec())?
                .with_display_name(display_name)
                .with_api_key_env(env_keys);
            Ok(Arc::new(provider))
        }
        #[cfg(not(feature = "provider-google"))]
        {
            Err(super::catalog::missing_adapter_error("google-vertex"))
        }
    } else if first.api == "openai-responses" {
        #[cfg(feature = "provider-openai")]
        {
            let env_keys = env_keys_for(provider_id, "openai", &["OPENAI_API_KEY"]);
            let provider = OpenAiResponsesProvider::new(provider_id, base_url, models.to_vec())?
                .with_display_name(display_name)
                .with_api_key_env(env_keys);
            Ok(Arc::new(provider))
        }
        #[cfg(not(feature = "provider-openai"))]
        {
            Err(super::catalog::missing_adapter_error("openai-responses"))
        }
    } else if first.api == "openai-codex-responses" {
        #[cfg(feature = "provider-openai")]
        {
            let provider =
                OpenAiCodexResponsesProvider::new(provider_id, base_url, models.to_vec())?
                    .with_display_name(display_name);
            Ok(Arc::new(provider))
        }
        #[cfg(not(feature = "provider-openai"))]
        {
            Err(super::catalog::missing_adapter_error(
                "openai-codex-responses",
            ))
        }
    } else if first.api == "azure-openai-responses" {
        #[cfg(feature = "provider-openai")]
        {
            let env_keys = env_keys_for(provider_id, "azure", &["AZURE_OPENAI_API_KEY"]);
            let provider =
                AzureOpenAiResponsesProvider::new(provider_id, base_url, models.to_vec())?
                    .with_display_name(display_name)
                    .with_api_key_env(env_keys);
            Ok(Arc::new(provider))
        }
        #[cfg(not(feature = "provider-openai"))]
        {
            Err(super::catalog::missing_adapter_error(
                "azure-openai-responses",
            ))
        }
    } else if first.api == "bedrock-converse-stream" {
        #[cfg(feature = "provider-bedrock")]
        {
            let provider =
                BedrockConverseStreamProvider::new(provider_id, base_url, models.to_vec())?
                    .with_display_name(display_name);
            Ok(Arc::new(provider))
        }
        #[cfg(not(feature = "provider-bedrock"))]
        {
            Err(super::catalog::missing_adapter_error(
                "bedrock-converse-stream",
            ))
        }
    } else if first.api == "mistral-conversations" {
        #[cfg(feature = "provider-mistral")]
        {
            let env_keys = env_keys_for(provider_id, "mistral", &["MISTRAL_API_KEY"]);
            let provider =
                MistralConversationsProvider::new(provider_id, base_url, models.to_vec())?
                    .with_display_name(display_name)
                    .with_api_key_env(env_keys);
            Ok(Arc::new(provider))
        }
        #[cfg(not(feature = "provider-mistral"))]
        {
            Err(super::catalog::missing_adapter_error(
                "mistral-conversations",
            ))
        }
    } else {
        #[cfg(feature = "provider-openai")]
        {
            let env_keys: Vec<String> = env_api_keys::all()
                .into_iter()
                .find(|(id, _)| *id == provider_id)
                .map(|(_, keys)| keys.iter().map(|key| (*key).to_string()).collect())
                .unwrap_or_else(|| vec!["OPENAI_API_KEY".to_string()]);
            let provider = OpenAiCompatibleProvider::new(provider_id, base_url, models.to_vec())?
                .with_display_name(display_name)
                .with_api_key_env(env_keys);
            Ok(Arc::new(provider))
        }
        #[cfg(not(feature = "provider-openai"))]
        {
            Err(super::catalog::missing_adapter_error(&first.api))
        }
    }
}

/// Resolve the api-key env names for a provider from the runtime env-api-key
/// registry, falling back to the given defaults. Used by the openai-family
/// and mistral arms.
#[cfg_attr(
    not(any(feature = "provider-openai", feature = "provider-mistral")),
    allow(dead_code)
)]
fn env_keys_for(provider_id: &str, registry_id: &str, defaults: &[&str]) -> Vec<String> {
    env_api_keys::all()
        .into_iter()
        .find(|(id, _)| *id == registry_id || *id == provider_id)
        .map(|(_, keys)| keys.iter().map(|key| (*key).to_string()).collect())
        .unwrap_or_else(|| defaults.iter().map(|name| (*name).to_string()).collect())
}

// ---------------------------------------------------------------------------
// Built-in catalog providers
// ---------------------------------------------------------------------------

/// A built-in catalog provider: one runtime unit that dispatches each model to
/// the protocol adapter for its api id. Catalog entries whose api has no
/// adapter in this build (no dedicated adapter yet, or the `provider-…`
/// feature that compiles it is disabled) are selectable and configurable, but
/// streaming yields a clear error naming the missing feature instead of
/// constructing a foreign adapter.
pub struct CatalogProviderEntry {
    provider_id: String,
    display_name: String,
    /// api id -> streaming adapter (absent = unsupported in this build).
    adapters: BTreeMap<String, Option<ProviderBox>>,
    models: Vec<Model>,
}

impl CatalogProviderEntry {
    /// Build the runtime provider for one parsed catalog entry.
    pub fn from_catalog(entry: &super::catalog::BuiltinProvider) -> AiResult<ProviderBox> {
        Self::from_models(&entry.id, &entry.display_name, &entry.models)
    }

    /// Build a catalog entry from explicit api-id → adapter pairs.
    ///
    /// [`Self::from_models`] is the production construction path; this seam
    /// lets tests (and embedders dispatching to their own adapters) exercise
    /// the catalog wrapper without constructing a network adapter.
    #[doc(hidden)]
    pub fn from_adapters(
        provider_id: impl Into<String>,
        display_name: impl Into<String>,
        adapters: BTreeMap<String, Option<ProviderBox>>,
        models: Vec<Model>,
    ) -> ProviderBox {
        Arc::new(Self {
            provider_id: provider_id.into(),
            display_name: display_name.into(),
            adapters,
            models,
        })
    }

    /// Build the runtime provider for a (possibly models.json-composed) model
    /// list, grouping by api id like [`Self::from_catalog`].
    pub fn from_models(
        provider_id: &str,
        display_name: &str,
        models: &[Model],
    ) -> AiResult<ProviderBox> {
        let mut groups: BTreeMap<&str, Vec<Model>> = BTreeMap::new();
        for model in models {
            groups
                .entry(model.api.as_str())
                .or_default()
                .push(model.clone());
        }
        let mut adapters: BTreeMap<String, Option<ProviderBox>> = BTreeMap::new();
        for (api, models) in groups {
            // `openai-chat` is the fallback api id for models.json/dynamic
            // entries; it needs the OpenAI-compatible adapter, so it follows
            // `provider-openai` just like `supported_apis()` does. With the
            // feature off the api maps to `None` (streaming reports the
            // disabled feature) instead of failing provider construction.
            let supported = super::catalog::supported_apis().contains(&api)
                || (api == "openai-chat" && cfg!(feature = "provider-openai"));
            let adapter = if supported {
                Some(build_provider_from_models(
                    provider_id,
                    display_name,
                    &models,
                )?)
            } else {
                None
            };
            adapters.insert(api.to_string(), adapter);
        }
        Ok(Arc::new(Self {
            provider_id: provider_id.to_string(),
            display_name: display_name.to_string(),
            adapters,
            models: models.to_vec(),
        }))
    }

    fn env_keys(&self) -> Vec<String> {
        let bearer: Vec<&str> = env_api_keys::bearer_env_vars(&self.provider_id);
        let from_registry = env_api_keys::all()
            .into_iter()
            .find(|(id, _)| *id == self.provider_id)
            .map(|(_, keys)| {
                keys.iter()
                    // Bearer env vars resolve to headers, not api keys.
                    .filter(|key| !bearer.contains(key))
                    .map(|key| (*key).to_string())
                    .collect::<Vec<_>>()
            });
        match from_registry {
            Some(keys) => keys,
            // Fall back to the per-provider default env var
            // (`<PROVIDER_ID>_API_KEY`, snake-cased), so models.json-only
            // providers keep an ambient env auth surface.
            None => vec![default_env_var_for_provider(&self.provider_id)],
        }
    }
}

/// The per-provider default api-key env var: the provider id uppercased and
/// non-alphanumerics turned into underscores (upstream's
/// `SCREAMING_SNAKE_CASE` env-var convention).
fn default_env_var_for_provider(provider_id: &str) -> String {
    let mut var = String::with_capacity(provider_id.len() + "_API_KEY".len());
    for ch in provider_id.chars() {
        if ch.is_ascii_alphanumeric() {
            var.push(ch.to_ascii_uppercase());
        } else {
            var.push('_');
        }
    }
    var.push_str("_API_KEY");
    var
}

impl crate::ai::provider::Provider for CatalogProviderEntry {
    fn id(&self) -> &str {
        &self.provider_id
    }

    fn name(&self) -> &str {
        &self.display_name
    }

    fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }

    /// Credential-specific availability (upstream `filterModels`): GitHub
    /// Copilot's API exposes the model ids a subscription may use in the
    /// OAuth credential's `availableModelIds` extra field; when present, only
    /// those models are available. Every other provider lists its full
    /// catalog.
    fn filter_models(
        &self,
        models: Vec<Model>,
        credential: Option<&crate::ai::auth::StoredCredential>,
    ) -> Vec<Model> {
        if self.provider_id == "github-copilot" {
            if let Some(crate::ai::auth::StoredCredential::OAuth(credential)) = credential {
                if let Some(ids) = credential
                    .extra
                    .get("availableModelIds")
                    .and_then(serde_json::Value::as_array)
                {
                    let available: std::collections::BTreeSet<&str> =
                        ids.iter().filter_map(serde_json::Value::as_str).collect();
                    return models
                        .into_iter()
                        .filter(|model| available.contains(model.id.as_str()))
                        .collect();
                }
            }
        }
        models
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        let env_keys = self.env_keys();
        let bearer_env_vars: Vec<String> = env_api_keys::bearer_env_vars(&self.provider_id)
            .into_iter()
            .map(str::to_string)
            .collect();
        if env_keys.is_empty() && bearer_env_vars.is_empty() {
            Vec::new()
        } else {
            vec![
                ApiKeyAuth::new(format!("{} API key", self.display_name), env_keys)
                    .with_bearer_env_vars(bearer_env_vars),
            ]
        }
    }

    fn oauth_flow(&self) -> Option<crate::ai::auth::OAuthFlowBox> {
        crate::ai::auth::oauth_flow_for(&self.provider_id, None)
    }

    fn stream(
        &self,
        model: &Model,
        context: crate::ai::types::Context,
        options: crate::ai::types::StreamOptions,
    ) -> crate::ai::event_stream::AssistantMessageEventStream {
        match self.adapters.get(model.api.as_str()) {
            Some(Some(adapter)) => adapter.stream(model, context, options),
            Some(None) => {
                catalog_stream_error(model, super::catalog::unsupported_api_error(&model.api))
            }
            None => catalog_stream_error(
                model,
                format!(
                    "Unknown api '{}' for provider {}",
                    model.api, self.provider_id
                ),
            ),
        }
    }

    /// Delegate the SHARED transcript handle to the adapter's shared entry
    /// point. The catalog rewrites nothing (no model/context substitution), so
    /// this is a straight passthrough: the adapter streams from the loop's
    /// allocation instead of materializing a transcript-sized owned copy in
    /// the trait default.
    fn stream_shared(
        &self,
        model: &Model,
        context: Arc<crate::ai::types::Context>,
        options: crate::ai::types::StreamOptions,
    ) -> crate::ai::event_stream::AssistantMessageEventStream {
        match self.adapters.get(model.api.as_str()) {
            Some(Some(adapter)) => adapter.stream_shared(model, context, options),
            Some(None) => {
                catalog_stream_error(model, super::catalog::unsupported_api_error(&model.api))
            }
            None => catalog_stream_error(
                model,
                format!(
                    "Unknown api '{}' for provider {}",
                    model.api, self.provider_id
                ),
            ),
        }
    }
}

/// Terminate a catalog request that cannot be dispatched with a typed `Error`
/// event (unknown/catalog-uncompiled api).
fn catalog_stream_error(
    model: &Model,
    message_text: String,
) -> crate::ai::event_stream::AssistantMessageEventStream {
    let output = crate::ai::event_stream::AssistantMessageEventStream::new();
    let message = crate::ai::provider::error_message(model, &message_text);
    output.push(
        crate::ai::event_stream::AssistantMessageEventStream::error_event(
            crate::ai::types::ErrorReason::Error,
            message.clone(),
        ),
    );
    output.end(Some(Arc::new(message)));
    output
}
