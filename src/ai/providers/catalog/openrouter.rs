//! OpenRouter per-key catalog personalization (`#10672`, upstream
//! `packages/ai/src/providers/openrouter-api.ts` + `personalizeOpenRouterCatalog`).
//!
//! With an OpenRouter key, pi lists only the chat models that key may use,
//! taking each model's context size, max output and prices from
//! `GET {baseUrl}/models/user`. Image and classifier models are kept (in pir
//! they are separate catalog kinds, so the chat filter leaves them alone).
//! The combined list replaces the OpenRouter catalog and is persisted, so it
//! is also used offline. A base URL configured in `models.json` (regional
//! endpoints such as `https://us.openrouter.ai/api/v1`) is used for the
//! request.
//!
//! The same price conversion backs the offline catalog generator
//! (upstream `scripts/openrouter-catalog.ts`): missing, invalid and negative
//! prices — OpenRouter reports `-1` for router models such as
//! `openrouter/auto`, whose price depends on the model they pick — become the
//! fallback (0 for base rates).

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{json, Value};

use crate::ai::error::{AiError, AiResult};
use crate::ai::types::{Model, ModelCost, ModelCostRates, ModelCostTier};

/// Default OpenRouter API base URL (upstream `OPENROUTER_BASE_URL`).
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Timeout of one `GET /models/user` request (upstream
/// `OPENROUTER_USER_MODELS_TIMEOUT_MS`).
pub const OPENROUTER_USER_MODELS_TIMEOUT: Duration = Duration::from_secs(15);

/// Environment variable holding the OpenRouter API key (upstream
/// `envApiKeyAuth("OpenRouter API key", ["OPENROUTER_API_KEY"])`).
pub const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// Round a $/1M rate to six decimals (upstream `Number(x.toFixed(6))`).
fn round_cost(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

/// Read one pricing string: JSON strings and numbers both parse (upstream
/// `Number.parseFloat`).
fn price_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Convert one $/token price to $/1M tokens (upstream `perMillion`). 0 means
/// free; missing, invalid and negative prices use the fallback — OpenRouter
/// reports `-1` for router models such as `openrouter/auto`.
pub fn per_million(value: Option<&Value>, fallback: f64) -> f64 {
    match price_number(value) {
        Some(per_token) if per_token >= 0.0 => round_cost(per_token * 1_000_000.0),
        _ => fallback,
    }
}

/// Convert an OpenRouter `pricing` object into `ModelCost` (upstream
/// `openRouterCost`): prompt-length overrides become request-wide tiers, a
/// rate a tier does not list keeps the base price, time-of-day overrides are
/// skipped (ModelCost cannot express them).
pub fn openrouter_cost(pricing: Option<&Value>) -> ModelCost {
    let base = ModelCostRates {
        input: per_million(pricing.and_then(|pricing| pricing.get("prompt")), 0.0),
        output: per_million(pricing.and_then(|pricing| pricing.get("completion")), 0.0),
        cache_read: per_million(
            pricing.and_then(|pricing| pricing.get("input_cache_read")),
            0.0,
        ),
        cache_write: per_million(
            pricing.and_then(|pricing| pricing.get("input_cache_write")),
            0.0,
        ),
    };
    let tiers = pricing
        .and_then(|pricing| pricing.get("overrides"))
        .and_then(Value::as_array)
        .map(|overrides| {
            overrides
                .iter()
                .filter(|override_| {
                    // Only prompt-length overrides can be expressed.
                    override_
                        .get("min_prompt_tokens")
                        .is_some_and(|v| !v.is_null())
                        && ["utc_start", "utc_end", "utc_days"]
                            .iter()
                            .all(|key| override_.get(*key).is_none_or(Value::is_null))
                })
                .filter_map(|override_| {
                    let input_tokens_above =
                        override_.get("min_prompt_tokens").and_then(Value::as_u64)?;
                    Some(ModelCostTier {
                        input_tokens_above,
                        rates: ModelCostRates {
                            input: per_million(override_.get("prompt"), base.input),
                            output: per_million(override_.get("completion"), base.output),
                            cache_read: per_million(
                                override_.get("input_cache_read"),
                                base.cache_read,
                            ),
                            cache_write: per_million(
                                override_.get("input_cache_write"),
                                base.cache_write,
                            ),
                        },
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    ModelCost { rates: base, tiers }
}

/// One `GET /models/user` entry (the fields pi reads; upstream
/// `OpenRouterModel`).
#[derive(Debug, Clone, serde::Deserialize)]
struct OpenRouterUserModel {
    id: String,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    top_provider: Option<OpenRouterTopProvider>,
    #[serde(default)]
    pricing: Option<Value>,
}

/// The `top_provider` block of a user-models entry.
#[derive(Debug, Clone, serde::Deserialize)]
struct OpenRouterTopProvider {
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

/// A non-zero limit wins; a missing/zero/null limit keeps the catalog value
/// (upstream `||` chains).
fn limit_or(primary: Option<u64>, secondary: Option<u64>, fallback: u64) -> u64 {
    primary
        .filter(|value| *value > 0)
        .or_else(|| secondary.filter(|value| *value > 0))
        .unwrap_or(fallback)
}

/// Filter `catalog` to the chat models `key` may use, with the key's context
/// size, max output and prices (upstream `personalizeOpenRouterCatalog`).
///
/// `GET {base_url}/models/user` that rejects the key (401/403) or does not
/// exist (404) — for example a proxy configured in `models.json` — returns
/// the catalog unchanged; any other non-2xx status and an invalid body are
/// errors. A missing key returns the catalog unchanged.
pub async fn personalize_openrouter_catalog(
    catalog: Vec<Model>,
    base_url: &str,
    key: Option<&str>,
    timeout: Duration,
) -> AiResult<Vec<Model>> {
    let Some(key) = key.filter(|key| !key.is_empty()) else {
        return Ok(catalog);
    };
    let url = format!("{}/models/user", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .build()
        .map_err(|error| AiError::Http(error.to_string()))?;
    let response = client
        .get(&url)
        .header("accept", "application/json")
        .bearer_auth(key)
        .send()
        .await
        .map_err(|error| AiError::Http(error.to_string()))?;
    let status = response.status().as_u16();
    if status == 401 || status == 403 || status == 404 {
        return Ok(catalog);
    }
    if !(200..300).contains(&status) {
        return Err(AiError::NonRetryable(format!(
            "OpenRouter model list request failed: {status}"
        )));
    }
    let body = response
        .text()
        .await
        .map_err(|error| AiError::Http(error.to_string()))?;
    let parsed: Value = serde_json::from_str(&body)
        .map_err(|_| AiError::NonRetryable("Invalid OpenRouter model list response".to_string()))?;
    let Some(entries) = parsed.get("data").and_then(Value::as_array) else {
        return Err(AiError::NonRetryable(
            "Invalid OpenRouter model list response".to_string(),
        ));
    };
    let mut allowed: BTreeMap<String, OpenRouterUserModel> = BTreeMap::new();
    for entry in entries {
        if let Ok(model) = serde_json::from_value::<OpenRouterUserModel>(entry.clone()) {
            if !model.id.is_empty() {
                allowed.insert(model.id.clone(), model);
            }
        }
    }
    Ok(catalog
        .into_iter()
        .filter_map(|model| {
            let entry = allowed.get(&model.id)?;
            let top_provider = entry.top_provider.as_ref();
            Some(Model {
                context_window: limit_or(
                    top_provider.and_then(|top| top.context_length),
                    entry.context_length,
                    model.context_window,
                ),
                max_tokens: limit_or(
                    top_provider.and_then(|top| top.max_completion_tokens),
                    None,
                    model.max_tokens,
                ),
                cost: match &entry.pricing {
                    Some(pricing) => openrouter_cost(Some(pricing)),
                    None => model.cost,
                },
                ..model
            })
        })
        .collect())
}

/// The credential `GET /models/user` uses: the stored api-key credential,
/// else the stored OAuth access token, else `$OPENROUTER_API_KEY` (upstream
/// resolves the credential the provider is registered with).
pub fn openrouter_api_key() -> Option<String> {
    let stored = crate::ai::auth::FileAuthStorage::default_storage()
        .read_stored("openrouter")
        .ok()
        .flatten();
    if let Some(credential) = stored {
        let key = match credential {
            crate::ai::auth::StoredCredential::ApiKey(credential) => credential.key,
            crate::ai::auth::StoredCredential::OAuth(credential) => Some(credential.access),
        };
        if key.as_ref().is_some_and(|key| !key.is_empty()) {
            return key;
        }
    }
    std::env::var(OPENROUTER_API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
}

/// The provider's configured base URL (`models.json`
/// `providers.openrouter.baseUrl`, regional endpoints), else
/// [`OPENROUTER_BASE_URL`].
pub fn configured_base_url() -> String {
    let path = crate::ai::providers::agent_dir().join("models.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| crate::ai::providers::parse_pi_config(&contents).ok())
        .and_then(|config| {
            config
                .providers
                .get("openrouter")
                .and_then(|provider| provider.base_url.clone())
        })
        .filter(|base_url| !base_url.trim().is_empty())
        .unwrap_or_else(|| OPENROUTER_BASE_URL.to_string())
}

/// Serialize one model into the models-store wire shape (camelCase, the
/// `RemoteCatalogModel`/embedded-catalog contract) so a personalized list
/// restores offline.
pub fn model_wire_value(model: &Model) -> Value {
    let cost = &model.cost;
    let mut value = json!({
        "id": model.id,
        "type": "chat",
        "name": model.name,
        "api": model.api,
        "baseUrl": model.base_url,
        "reasoning": model.reasoning,
        "input": model.input,
        "cost": {
            "input": cost.rates.input,
            "output": cost.rates.output,
            "cacheRead": cost.rates.cache_read,
            "cacheWrite": cost.rates.cache_write,
            "tiers": cost.tiers.iter().map(|tier| json!({
                "inputTokensAbove": tier.input_tokens_above,
                "input": tier.rates.input,
                "output": tier.rates.output,
                "cacheRead": tier.rates.cache_read,
                "cacheWrite": tier.rates.cache_write,
            })).collect::<Vec<_>>(),
        },
        "contextWindow": model.context_window,
        "maxTokens": model.max_tokens,
    });
    if let Some(thinking_level_map) = &model.thinking_level_map {
        value["thinkingLevelMap"] = serde_json::to_value(thinking_level_map).unwrap_or(Value::Null);
    }
    if let Some(headers) = &model.headers {
        value["headers"] = serde_json::to_value(headers).unwrap_or(Value::Null);
    }
    if let Some(compat) = &model.compat {
        value["compat"] = serde_json::to_value(compat).unwrap_or(Value::Null);
    }
    value
}

/// [`model_wire_value`] over a whole list.
pub fn model_wire_values(models: &[Model]) -> Vec<Value> {
    models.iter().map(model_wire_value).collect()
}
