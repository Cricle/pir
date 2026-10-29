//! Azure OpenAI Responses configuration.
//!
//! Rust equivalent of upstream `azure-openai-config.ts`: deployment-based URL
//! construction from `AZURE_OPENAI_RESOURCE_NAME` / `AZURE_OPENAI_BASE_URL` /
//! `AZURE_OPENAI_API_VERSION` / `AZURE_OPENAI_DEPLOYMENT_NAME_MAP` (options
//! env first, then process env), Azure host path normalization to
//! `/openai/v1`, and model id -> deployment name resolution.

use serde_json::Value;

use crate::ai::api::provider_env_value;
use crate::ai::types::{Model, ProviderEnv, StreamOptions};

/// Default Azure API version (upstream `DEFAULT_AZURE_API_VERSION`).
const DEFAULT_AZURE_API_VERSION: &str = "v1";

/// Parse `modelId=deployment,...` pairs (upstream `parseDeploymentNameMap`).
fn parse_deployment_name_map(value: Option<&str>) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    let Some(value) = value else {
        return map;
    };
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut parts = entry.splitn(2, '=');
        let (Some(model_id), Some(deployment)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (model_id, deployment) = (model_id.trim(), deployment.trim());
        if model_id.is_empty() || deployment.is_empty() {
            continue;
        }
        map.insert(model_id.to_string(), deployment.to_string());
    }
    map
}

/// Resolve the deployment name for a model (upstream `resolveDeploymentName`).
/// Public for tests/.
pub fn resolve_deployment_name(model: &Model, env: Option<&ProviderEnv>) -> String {
    resolve_deployment_name_with_options(model, env, None)
}

/// [`resolve_deployment_name`] with the request-level
/// `AzureRequestOptions.deployment_name` override first.
pub fn resolve_deployment_name_with_options(
    model: &Model,
    env: Option<&ProviderEnv>,
    azure: Option<&crate::ai::types::AzureRequestOptions>,
) -> String {
    if let Some(deployment) = azure
        .and_then(|azure| azure.deployment_name.as_deref())
        .map(str::trim)
        .filter(|deployment| !deployment.is_empty())
    {
        return deployment.to_string();
    }
    let mapped = parse_deployment_name_map(
        provider_env_value("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", env).as_deref(),
    )
    .get(&model.id)
    .cloned();
    mapped.unwrap_or_else(|| model.id.clone())
}

/// Normalize an Azure base URL: Azure hosts get the `/openai/v1` base path so
/// the deployment segment and `api-version` query append correctly (upstream
/// `normalizeAzureBaseUrl`). Public for tests/.
pub fn normalize_azure_base_url(base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    // Minimal scheme://host[:port][/path] parsing (no external url crate).
    let (scheme, rest) = trimmed
        .split_once("://")
        .ok_or_else(|| format!("Invalid Azure OpenAI base URL: {base_url}"))?;
    if scheme != "http" && scheme != "https" {
        return Err(format!("Invalid Azure OpenAI base URL: {base_url}"));
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(format!("Invalid Azure OpenAI base URL: {base_url}"));
    }
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_azure_host = host.ends_with(".openai.azure.com")
        || host.ends_with(".cognitiveservices.azure.com")
        || host.ends_with(".ai.azure.com");
    let path = path.trim_end_matches('/');
    if is_azure_host
        && (path.is_empty() || path == "/" || path == "/openai" || path == "/openai/v1/responses")
    {
        return Ok(format!("{scheme}://{authority}/openai/v1"));
    }
    Ok(format!("{scheme}://{authority}{path}"))
}

/// Resolve the Azure endpoint configuration (upstream `resolveAzureConfig`).
/// Public for tests/.
/// [`resolve_azure_base_url_with_options`] plus the api-version resolution.
fn resolve_azure_config_with_options(
    model: &Model,
    env: Option<&ProviderEnv>,
    azure: Option<&crate::ai::types::AzureRequestOptions>,
) -> Result<(String, String), String> {
    let base_url = resolve_azure_base_url_with_options(model, env, azure)?;
    let api_version = azure
        .and_then(|azure| azure.api_version.as_deref())
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(str::to_string)
        .or_else(|| provider_env_value("AZURE_OPENAI_API_VERSION", env))
        .unwrap_or_else(|| DEFAULT_AZURE_API_VERSION.to_string());
    Ok((base_url, api_version))
}

/// Resolve the Azure base URL (upstream `resolveAzureBaseUrl`): explicit env
/// base URL, else resource-name default, else the model's catalog base URL.
pub fn resolve_azure_base_url(model: &Model, env: Option<&ProviderEnv>) -> Result<String, String> {
    resolve_azure_base_url_with_options(model, env, None)
}

/// [`resolve_azure_base_url`] with the request-level
/// `AzureRequestOptions.base_url` / `resource_name` overrides first.
pub fn resolve_azure_base_url_with_options(
    model: &Model,
    env: Option<&ProviderEnv>,
    azure: Option<&crate::ai::types::AzureRequestOptions>,
) -> Result<String, String> {
    let option_base = azure
        .and_then(|azure| azure.base_url.as_deref())
        .map(str::trim)
        .filter(|base| !base.is_empty())
        .map(str::to_string);
    let option_resource = azure
        .and_then(|azure| azure.resource_name.as_deref())
        .map(str::trim)
        .filter(|resource| !resource.is_empty());
    let env_base = provider_env_value("AZURE_OPENAI_BASE_URL", env)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let resource_name = provider_env_value("AZURE_OPENAI_RESOURCE_NAME", env);
    let resolved = if let Some(base) = option_base {
        base
    } else if let Some(resource) = option_resource {
        format!("https://{resource}.openai.azure.com/openai/v1")
    } else if let Some(base) = env_base {
        base
    } else if let Some(resource) = resource_name {
        format!("https://{resource}.openai.azure.com/openai/v1")
    } else if !model.base_url.is_empty() {
        model.base_url.clone()
    } else {
        return Err(
            "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or \
             AZURE_OPENAI_RESOURCE_NAME, or pass azureBaseUrl, azureResourceName, \
             or model.baseUrl."
                .to_string(),
        );
    };
    normalize_azure_base_url(&resolved)
}

/// The full `/responses` request URL for a deployment (the AzureOpenAI SDK's
/// deployment-based path plus the `api-version` query). Public for tests/.
pub fn azure_responses_url(model: &Model, env: Option<&ProviderEnv>) -> Result<String, String> {
    azure_responses_url_with_options(model, env, None)
}

/// [`azure_responses_url`] with request-level endpoint overrides.
pub fn azure_responses_url_with_options(
    model: &Model,
    env: Option<&ProviderEnv>,
    azure: Option<&crate::ai::types::AzureRequestOptions>,
) -> Result<String, String> {
    let (base_url, api_version) = resolve_azure_config_with_options(model, env, azure)?;
    let deployment = resolve_deployment_name_with_options(model, env, azure);
    Ok(format!(
        "{base_url}/deployments/{deployment}/responses?api-version={api_version}"
    ))
}

/// Build the Azure `/responses` request body (upstream
/// `azure-openai-responses.ts buildParams`): the shared codec with the
/// deployment name as the wire `model` value and no ChatGPT sign-in field
/// omissions. Public for tests/.
pub fn build_azure_params(
    model: &Model,
    context: &crate::ai::types::Context,
    options: &StreamOptions,
) -> Result<Value, crate::ai::error::AiError> {
    use crate::ai::api::openai_responses::responses_compat::ResponsesCompat;
    use crate::ai::api::openai_responses::shared::AZURE_TOOL_CALL_PROVIDERS;
    let mut compat = ResponsesCompat::from_model(model);
    // Azure defaults strict tool schemas on (upstream
    // `supportsStrictMode ?? true`).
    compat.supports_strict_mode = model
        .compat
        .as_ref()
        .and_then(|compat| compat.supports_strict_mode)
        .unwrap_or(true);
    // Azure rejects `prompt_cache_retention`; it only takes the cache key
    // (upstream azure-openai-responses.ts buildParams).
    compat.supports_prompt_cache_retention = false;
    let deployment =
        resolve_deployment_name_with_options(model, options.env.as_ref(), options.azure.as_ref());
    crate::ai::api::openai_responses::shared::build_responses_params(
        model,
        context,
        options,
        &compat,
        &deployment,
        AZURE_TOOL_CALL_PROVIDERS,
        false,
        // Azure does not use the GPT-6 configuration_update path.
        None,
    )
}
