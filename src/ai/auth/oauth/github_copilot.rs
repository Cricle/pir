//! GitHub Copilot OAuth flow.
//!
//! Rust equivalent of upstream `packages/ai/src/auth/oauth/github-copilot.ts`
//! (device flow + Copilot internal token exchange + proxy-ep base URL
//! derivation + the post-login model-catalog fetch/policy-enable step).
//! Pir filters models against its static catalog, which supplies the
//! upstream `GITHUB_COPILOT_MODELS` key set for the policy-enable pass.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use super::super::device_code::{poll_device_code_flow, DevicePoll};
use super::super::http::{get_json, post_form, redact_oauth_body_text};
use super::super::interaction::{AuthEvent, AuthInteraction, AuthPrompt};
use super::{LoginOptions, OAuthCredential, OAuthFlow};
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::ModelAuth;

/// Upstream decodes this from base64 (`Iv1.b507a08c87ecfe98`).
pub const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const COPILOT_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];
const COPILOT_API_VERSION: &str = "2026-06-01";

const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

/// Normalize a user-entered GitHub Enterprise URL/domain to a hostname.
pub fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    // Take the authority portion.
    let rest = with_scheme
        .strip_prefix("https://")
        .or_else(|| with_scheme.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn urls(domain: &str) -> (String, String, String) {
    (
        format!("https://{domain}/login/device/code"),
        format!("https://{domain}/login/oauth/access_token"),
        format!("https://api.{domain}/copilot_internal/v2/token"),
    )
}

/// Parse the `proxy-ep` from a Copilot token and convert it to the API base
/// URL (upstream `getBaseUrlFromToken`): `proxy.individual.githubcopilot.com`
/// becomes `https://api.individual.githubcopilot.com`.
pub fn base_url_from_token(token: &str) -> Option<String> {
    let marker = "proxy-ep=";
    let start = token.find(marker)? + marker.len();
    let rest = &token[start..];
    let end = rest.find(';').unwrap_or(rest.len());
    let proxy_host = &rest[..end];
    if proxy_host.is_empty() {
        return None;
    }
    // Upstream: `proxyHost.replace(/^proxy\./, "api.")` — the prefix is
    // rewritten to `api.`, not stripped.
    let api_host = match proxy_host.strip_prefix("proxy.") {
        Some(rest) => format!("api.{rest}"),
        None => proxy_host.to_string(),
    };
    Some(format!("https://{api_host}"))
}

/// The Copilot API base URL for a token/enterprise pair
/// (upstream `getGitHubCopilotBaseUrl`).
pub fn copilot_base_url(token: Option<&str>, enterprise_domain: Option<&str>) -> String {
    if let Some(token) = token {
        if let Some(url) = base_url_from_token(token) {
            return url;
        }
    }
    match enterprise_domain {
        Some(domain) => format!("https://copilot-api.{domain}"),
        None => "https://api.individual.githubcopilot.com".to_string(),
    }
}

struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
    expires_in: u64,
}

/// The verification URI is opened in the user's browser; only http(s) URLs are
/// trusted (upstream's `Untrusted verification_uri` guard).
pub fn trusted_http_url(value: &str) -> AiResult<String> {
    let ok = value.starts_with("https://") || value.starts_with("http://");
    if ok && !value.contains(char::is_whitespace) {
        Ok(value.to_string())
    } else {
        Err(AiError::Authentication(
            "Untrusted verification_uri in device code response".to_string(),
        ))
    }
}

async fn start_device_flow(domain: &str) -> AiResult<DeviceCodeResponse> {
    let (device_code_url, _, _) = urls(domain);
    let response = post_form(
        &device_code_url,
        &[("client_id", CLIENT_ID), ("scope", "read:user")],
    )
    .await?;
    if !response.ok() {
        return Err(AiError::Authentication(format!(
            "device code request failed (HTTP {}): {}",
            response.status,
            redact_oauth_body_text(&response.body)
        )));
    }
    let json: Value = serde_json::from_str(&response.body)
        .map_err(|_| AiError::Authentication("Invalid device code response".to_string()))?;
    let device_code = super::super::http::required_string(&json, "device_code")?;
    let user_code = super::super::http::required_string(&json, "user_code")?;
    let verification_uri = trusted_http_url(&super::super::http::required_string(
        &json,
        "verification_uri",
    )?)?;
    let expires_in = super::super::http::positive_number(&json, "expires_in")? as u64;
    let interval = json.get("interval").and_then(Value::as_u64);
    Ok(DeviceCodeResponse {
        device_code,
        user_code,
        verification_uri,
        interval,
        expires_in,
    })
}

async fn poll_for_github_access_token(
    domain: &str,
    device: &DeviceCodeResponse,
) -> AiResult<String> {
    let (_, access_token_url, _) = urls(domain);
    poll_device_code_flow(device.interval, Some(device.expires_in), true, || async {
        let response = post_form(
            &access_token_url,
            &[
                ("client_id", CLIENT_ID),
                ("device_code", &device.device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )
        .await?;
        if !response.ok() {
            return Ok(DevicePoll::Failed {
                message: format!(
                    "Device token request failed (HTTP {}): {}",
                    response.status,
                    redact_oauth_body_text(&response.body)
                ),
            });
        }
        let json: Value = serde_json::from_str(&response.body)
            .map_err(|_| AiError::Authentication("Invalid device token response".to_string()))?;
        if let Some(access_token) = json.get("access_token").and_then(Value::as_str) {
            return Ok(DevicePoll::Complete(access_token.to_string()));
        }
        let error = json.get("error").and_then(Value::as_str).unwrap_or("");
        match error {
            "authorization_pending" => Ok(DevicePoll::Pending),
            "slow_down" => Ok(DevicePoll::SlowDown {
                interval_seconds: json.get("interval").and_then(Value::as_u64),
            }),
            "" => Ok(DevicePoll::Failed {
                message: "Invalid device token response".to_string(),
            }),
            other => {
                let description = json
                    .get("error_description")
                    .and_then(Value::as_str)
                    .map(|description| format!(": {description}"))
                    .unwrap_or_default();
                Ok(DevicePoll::Failed {
                    message: format!("Device flow failed: {other}{description}"),
                })
            }
        }
    })
    .await
}

/// Exchange the GitHub access token for the Copilot API token
/// (upstream `refreshGitHubCopilotAccessToken`).
async fn copilot_token_from_access(
    github_access_token: &str,
    enterprise_domain: Option<&str>,
) -> AiResult<OAuthCredential> {
    let domain = enterprise_domain.unwrap_or("github.com");
    let (_, _, copilot_token_url) = urls(domain);
    let response = get_json(
        &copilot_token_url,
        Some(github_access_token),
        COPILOT_HEADERS,
    )
    .await?;
    if !response.ok() {
        return Err(AiError::Authentication(format!(
            "Copilot token request failed (HTTP {}): {}",
            response.status,
            redact_oauth_body_text(&response.body)
        )));
    }
    let json: Value = serde_json::from_str(&response.body)
        .map_err(|_| AiError::Authentication("Invalid Copilot token response".to_string()))?;
    let token = super::super::http::required_string(&json, "token")?;
    let expires_at = super::super::http::positive_number(&json, "expires_at")? as i64;
    let mut credential = OAuthCredential::new(
        token,
        github_access_token.to_string(),
        // `expires_at` is provider-controlled (Unix seconds): saturating
        // multiply/subtract so an absurd value cannot overflow.
        expires_at
            .saturating_mul(1000)
            .saturating_sub(EXPIRY_MARGIN_MS),
    );
    if let Some(domain) = enterprise_domain {
        credential.set_extra("enterpriseUrl", Some(Value::String(domain.to_string())));
    }
    Ok(credential)
}

fn copilot_enterprise_domain(credential: &OAuthCredential) -> Option<String> {
    credential
        .extra_str("enterpriseUrl")
        .filter(|value| !value.is_empty())
        .and_then(normalize_domain)
}

// ---------------------------------------------------------------------------
// Model catalog fetch + policy enable (upstream fetchGitHubCopilotModels /
// parseGitHubCopilotModelCatalog / enableGitHubCopilotModels)
// ---------------------------------------------------------------------------

/// The parsed `/models` catalog (upstream's return shape).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopilotModelCatalog {
    pub available_model_ids: Vec<String>,
    pub policy_model_ids: Vec<String>,
}

/// The built-in github-copilot catalog model ids (upstream
/// `GITHUB_COPILOT_MODELS` key set), used to gate the policy-enable pass.
fn builtin_copilot_model_ids() -> AiResult<Vec<String>> {
    let provider =
        crate::ai::providers::catalog::load_provider("github-copilot")?.ok_or_else(|| {
            AiError::Authentication(
                "the github-copilot catalog entry is missing from this build".to_string(),
            )
        })?;
    Ok(provider
        .models
        .iter()
        .map(|model| model.id.clone())
        .collect())
}

/// Parse the `/models` response (upstream `parseGitHubCopilotModelCatalog`):
/// tool-call-capable entries yield `availableModelIds` (picker-enabled and
/// not policy-disabled; Individual accounts fall back to policy-enabled
/// entries when no picker flag is set) and `policyModelIds` (unconfigured
/// entries the built-in catalog knows, which need a policy-enable POST).
pub fn parse_github_copilot_model_catalog(
    raw: &Value,
    allow_policy_fallback: bool,
) -> AiResult<CopilotModelCatalog> {
    let entries = raw
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AiError::Authentication("Invalid Copilot models response".to_string()))?;
    struct AccountModel {
        id: String,
        picker_enabled: bool,
        policy_state: Option<String>,
    }
    let mut account_models = Vec::new();
    for item in entries {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let supports = item
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("supports"));
        if supports
            .and_then(|supports| supports.get("tool_calls"))
            .and_then(Value::as_bool)
            == Some(false)
        {
            continue;
        }
        account_models.push(AccountModel {
            id: id.to_string(),
            picker_enabled: item.get("model_picker_enabled").and_then(Value::as_bool) == Some(true),
            policy_state: item
                .get("policy")
                .and_then(|policy| policy.get("state"))
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    let picker_model_ids: Vec<String> = account_models
        .iter()
        .filter(|model| model.picker_enabled && model.policy_state.as_deref() != Some("disabled"))
        .map(|model| model.id.clone())
        .collect();
    let use_policy_fallback = allow_policy_fallback && picker_model_ids.is_empty();
    let available_model_ids = if !picker_model_ids.is_empty() || !allow_policy_fallback {
        picker_model_ids
    } else {
        account_models
            .iter()
            .filter(|model| model.policy_state.as_deref() == Some("enabled"))
            .map(|model| model.id.clone())
            .collect()
    };
    let builtin_ids = builtin_copilot_model_ids()?;
    let policy_model_ids = account_models
        .iter()
        .filter(|model| {
            model.policy_state.as_deref() == Some("unconfigured")
                && builtin_ids.contains(&model.id)
                && (model.picker_enabled || use_policy_fallback)
        })
        .map(|model| model.id.clone())
        .collect();
    Ok(CopilotModelCatalog {
        available_model_ids,
        policy_model_ids,
    })
}

/// The per-request timeout applied to Copilot API calls
/// (upstream `AbortSignal.timeout(5000)`).
const COPILOT_REQUEST_TIMEOUT_SECS: u64 = 5;

fn parse_retry_delay_ms(retry_after: Option<&str>, retry: u32) -> Option<u64> {
    match retry_after {
        Some(value) => value
            .parse::<f64>()
            .ok()
            .map(|seconds| (seconds * 1000.0).max(0.0) as u64),
        None => Some(500u64.saturating_mul(1 << retry.min(20))),
    }
}

/// GET `{base}/models` with the 429 rate-limit retry budget
/// (upstream `fetchWithRateLimitRetry`).
async fn get_with_rate_limit_retry(
    url: &str,
    bearer: &str,
    extra_headers: &[(&str, &str)],
    max_retries: u32,
) -> AiResult<super::super::http::HttpResponseWithHeaders> {
    for retry in 0..=max_retries {
        let response =
            super::super::http::get_with_headers(url, Some(bearer), extra_headers).await?;
        if response.status != 429 || retry == max_retries {
            return Ok(response);
        }
        if let Some(delay) = parse_retry_delay_ms(response.header("retry-after"), retry) {
            tokio::time::sleep(std::time::Duration::from_millis(delay.min(5_000))).await;
        }
    }
    unreachable!("retry loop always returns")
}

/// Fetch the account's model catalog from an explicit base URL (the testable
/// core; the production caller derives the base URL from the token).
pub async fn fetch_models_at(
    base_url: &str,
    copilot_token: &str,
    max_retries: u32,
) -> AiResult<CopilotModelCatalog> {
    let url = format!("{base_url}/models");
    let mut headers = COPILOT_HEADERS.to_vec();
    headers.push(("X-GitHub-Api-Version", COPILOT_API_VERSION));
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(COPILOT_REQUEST_TIMEOUT_SECS),
        get_with_rate_limit_retry(&url, copilot_token, &headers, max_retries),
    )
    .await
    .map_err(|_| AiError::Authentication(format!("Copilot models request timed out: {url}")))??;
    if !response.ok() {
        return Err(AiError::Authentication(format!(
            "{} {}: {}",
            response.status, "HTTP", response.body
        )));
    }
    // Some Individual accounts return false for every picker flag despite
    // explicit enabled policies. Limit the fallback to that endpoint so other
    // account types keep strict picker semantics (upstream).
    let allow_policy_fallback = base_url == "https://api.individual.githubcopilot.com";
    parse_github_copilot_model_catalog(&response.json()?, allow_policy_fallback)
}

/// Fetch the model catalog for a token/enterprise pair.
pub async fn fetch_github_copilot_models(
    copilot_token: &str,
    enterprise_domain: Option<&str>,
    max_retries: u32,
) -> AiResult<CopilotModelCatalog> {
    let base_url = copilot_base_url(Some(copilot_token), enterprise_domain);
    fetch_models_at(&base_url, copilot_token, max_retries).await
}

/// POST `{base}/models/{id}/policy` with `{state:"enabled"}` (upstream
/// `enableGitHubCopilotModel`). Returns whether the policy update succeeded;
/// a 429 after retries is an error that stops the batch.
pub async fn enable_model_at(base_url: &str, token: &str, model_id: &str) -> AiResult<bool> {
    let url = format!("{base_url}/models/{model_id}/policy");
    let mut headers: Vec<(&str, String)> = COPILOT_HEADERS
        .iter()
        .map(|(name, value)| (*name, value.to_string()))
        .collect();
    headers.push(("Authorization", format!("Bearer {token}")));
    headers.push(("openai-intent", "chat-policy".to_string()));
    headers.push(("x-interaction-type", "chat-policy".to_string()));
    let header_refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(COPILOT_REQUEST_TIMEOUT_SECS),
        super::super::http::post_json_with_headers(
            &url,
            &serde_json::json!({ "state": "enabled" }),
            &header_refs,
        ),
    )
    .await
    .map_err(|_| AiError::Authentication(format!("Copilot policy request timed out: {url}")))??;
    if response.status == 429 {
        return Err(AiError::Authentication(format!(
            "{} HTTP: {}",
            response.status, response.body
        )));
    }
    Ok(response.ok())
}

/// Enable the requested models and return the successful ids. Policy updates
/// are best effort; exhausted rate limiting stops the batch (upstream
/// `enableGitHubCopilotModels`).
pub async fn enable_github_copilot_models(
    token: &str,
    model_ids: &[String],
    enterprise_domain: Option<&str>,
) -> AiResult<Vec<String>> {
    let base_url = copilot_base_url(Some(token), enterprise_domain);
    let mut enabled = Vec::new();
    for model_id in model_ids {
        match enable_model_at(&base_url, token, model_id).await {
            Ok(true) => enabled.push(model_id.clone()),
            Ok(false) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(enabled)
}

/// Store the account's available model ids on the credential (upstream
/// `availableModelIds`).
pub fn set_available_model_ids(credential: &mut OAuthCredential, ids: Vec<String>) {
    let ids: std::collections::BTreeSet<String> = ids.into_iter().collect();
    credential.set_extra(
        "availableModelIds",
        Some(Value::Array(ids.into_iter().map(Value::String).collect())),
    );
}

/// The GitHub Copilot OAuth flow.
pub struct GitHubCopilotOAuth;

impl OAuthFlow for GitHubCopilotOAuth {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
        _options: &'a LoginOptions,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move {
            let input = interaction
                .prompt(AuthPrompt::Text {
                    message: "GitHub Enterprise URL/domain (blank for github.com)".to_string(),
                    placeholder: Some("company.ghe.com".to_string()),
                })
                .await?;
            let trimmed = input.trim();
            let enterprise_domain = normalize_domain(&input);
            if !trimmed.is_empty() && enterprise_domain.is_none() {
                return Err(AiError::Authentication(
                    "Invalid GitHub Enterprise URL/domain".to_string(),
                ));
            }
            let domain = enterprise_domain
                .clone()
                .unwrap_or_else(|| "github.com".to_string());

            let device = start_device_flow(&domain).await?;
            interaction.notify(AuthEvent::DeviceCode {
                user_code: device.user_code.clone(),
                verification_uri: device.verification_uri.clone(),
                interval_seconds: device.interval,
                expires_in_seconds: Some(device.expires_in),
            });
            let github_access_token = poll_for_github_access_token(&domain, &device).await?;
            let mut credential =
                copilot_token_from_access(&github_access_token, enterprise_domain.as_deref())
                    .await?;
            // Post-login model catalog fetch + policy enable (upstream
            // loginGitHubCopilot).
            let models =
                fetch_github_copilot_models(&credential.access, enterprise_domain.as_deref(), 2)
                    .await?;
            let mut available = models.available_model_ids;
            if !models.policy_model_ids.is_empty() {
                interaction.notify(AuthEvent::Progress {
                    message: "Enabling models...".to_string(),
                });
                match enable_github_copilot_models(
                    &credential.access,
                    &models.policy_model_ids,
                    enterprise_domain.as_deref(),
                )
                .await
                {
                    Ok(enabled) => available.extend(enabled),
                    Err(error) => return Err(error),
                }
            }
            set_available_model_ids(&mut credential, available);
            Ok(credential)
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move {
            let mut credential = copilot_token_from_access(
                &credential.refresh,
                copilot_enterprise_domain(credential).as_deref(),
            )
            .await?;
            // Upstream refreshGitHubCopilotToken re-reads the model catalog
            // without a retry budget.
            let models = fetch_github_copilot_models(
                &credential.access,
                copilot_enterprise_domain(&credential).as_deref(),
                0,
            )
            .await?;
            set_available_model_ids(&mut credential, models.available_model_ids);
            Ok(credential)
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            base_url: Some(copilot_base_url(
                Some(&credential.access),
                copilot_enterprise_domain(credential).as_deref(),
            )),
            headers: None,
        })
    }
}
