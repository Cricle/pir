//! Meta Model API OAuth flow.
//!
//! Rust equivalent of upstream `packages/ai/src/auth/oauth/meta.ts`: RFC 8628
//! device authorization against `https://auth.meta.com`; the resulting
//! identity token (stored as `refresh`) is exchanged for a Model API key
//! (stored as `access`) via the Muse Code key-mint endpoint. Refresh re-mints
//! the key from the stored identity token; a 401/403 means re-login.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use super::super::device_code::{poll_device_code_flow, DevicePoll};
use super::super::http::post_form;
use super::super::interaction::{AuthEvent, AuthInteraction};
use super::{now_ms, LoginOptions, OAuthCredential, OAuthFlow};
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::ModelAuth;

/// Muse Code CLI client id.
pub const CLIENT_ID: &str = "1031625952748946";
const DEVICE_AUTHORIZATION_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
pub const DEVICE_TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
const API_KEY_MINT_URL: &str = "https://api.meta.ai/muse-code/key";
const API_KEY_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;

fn error_detail(json: &Value) -> String {
    for key in ["error_description", "detail", "message", "error"] {
        if let Some(value) = json.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return format!(": {}", value.trim());
            }
        }
    }
    String::new()
}

/// The verification URI is opened in the user's browser; only http(s) URLs are
/// trusted.
fn trusted_http_url(value: &str) -> Option<String> {
    if (value.starts_with("https://") || value.starts_with("http://"))
        && !value.contains(char::is_whitespace)
    {
        Some(value.to_string())
    } else {
        None
    }
}

struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval_seconds: Option<u64>,
    expires_in_seconds: Option<u64>,
}

async fn start_device_authorization() -> AiResult<DeviceAuthorization> {
    let response = post_form(DEVICE_AUTHORIZATION_URL, &[("client_id", CLIENT_ID)]).await?;
    let json: Value = response.json().unwrap_or(Value::Null);
    if !response.ok() {
        return Err(AiError::Authentication(format!(
            "Meta device authorization failed with status {}{}",
            response.status,
            error_detail(&json)
        )));
    }
    let device_code = json
        .get("device_code")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let user_code = json
        .get("user_code")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let verification_uri = json
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .and_then(trusted_http_url)
        .or_else(|| {
            json.get("verification_uri")
                .and_then(Value::as_str)
                .and_then(trusted_http_url)
        });
    match (device_code, user_code, verification_uri) {
        (Some(device_code), Some(user_code), Some(verification_uri)) => Ok(DeviceAuthorization {
            device_code,
            user_code,
            verification_uri,
            interval_seconds: json
                .get("interval")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0),
            expires_in_seconds: json
                .get("expires_in")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0),
        }),
        _ => Err(AiError::Authentication(format!(
            "Invalid Meta device authorization response: {}",
            serde_json::to_string(&json).unwrap_or_default()
        ))),
    }
}

async fn poll_for_identity_token(device: &DeviceAuthorization) -> AiResult<String> {
    poll_device_code_flow(
        device.interval_seconds,
        device.expires_in_seconds,
        true,
        || async {
            let response = post_form(
                DEVICE_TOKEN_URL,
                &[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", &device.device_code),
                    ("client_id", CLIENT_ID),
                ],
            )
            .await?;
            let json: Value = response.json().unwrap_or(Value::Null);
            if response.ok() {
                if let Some(access_token) = json
                    .get("access_token")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    return Ok(DevicePoll::Complete(access_token.to_string()));
                }
            }
            match json.get("error").and_then(Value::as_str) {
                Some("authorization_pending") => Ok(DevicePoll::Pending),
                Some("slow_down") => Ok(DevicePoll::SlowDown {
                    interval_seconds: json
                        .get("interval")
                        .and_then(Value::as_u64)
                        .filter(|value| *value > 0),
                }),
                Some("access_denied") => Ok(DevicePoll::Failed {
                    message: "Meta login was denied.".to_string(),
                }),
                Some("expired_token") => Ok(DevicePoll::Failed {
                    message: "Meta device authorization expired. Please restart login.".to_string(),
                }),
                _ => Ok(DevicePoll::Failed {
                    message: format!(
                        "Meta device token request failed with status {}{}",
                        response.status,
                        error_detail(&json)
                    ),
                }),
            }
        },
    )
    .await
}

/// Exchange an identity token for a Model API key (valid for about a day).
async fn mint_api_key(identity_token: &str) -> AiResult<OAuthCredential> {
    let response = post_json_with_headers(API_KEY_MINT_URL, identity_token).await?;
    let json: Value = response.json().unwrap_or(Value::Null);
    if response.status == 401 || response.status == 403 {
        // The identity token is not renewable; only a fresh device flow helps.
        return Err(AiError::Authentication(format!(
            "Meta session expired (status {}). Run `/login meta` to sign in again.{}",
            response.status,
            error_detail(&json)
        )));
    }
    if !response.ok() {
        return Err(AiError::Authentication(format!(
            "Meta API key mint failed with status {}{}",
            response.status,
            error_detail(&json)
        )));
    }
    match json.get("api_key").and_then(Value::as_str) {
        Some(api_key) if !api_key.is_empty() => Ok(OAuthCredential::new(
            api_key,
            identity_token,
            now_ms() + API_KEY_LIFETIME_MS,
        )),
        _ => {
            let action_url = json
                .get("action_url")
                .and_then(Value::as_str)
                .and_then(trusted_http_url);
            Err(AiError::Authentication(format!(
                "Meta did not issue an API key.{}",
                action_url
                    .map(|url| format!(" Complete setup at {url}"))
                    .unwrap_or_default()
            )))
        }
    }
}

/// POST the empty JSON object with the identity bearer and the Meta API
/// version header, on the shared bounded OAuth HTTP helper.
async fn post_json_with_headers(
    url: &str,
    bearer: &str,
) -> AiResult<super::super::http::HttpResponse> {
    let authorization = format!("Bearer {bearer}");
    let headers = [
        ("Authorization", authorization.as_str()),
        ("x-api-version", "1.0.0"),
    ];
    super::super::http::post_json_with_headers(url, &serde_json::json!({}), &headers)
        .await
        .map(super::super::http::HttpResponse::from)
}

async fn login(interaction: &dyn AuthInteraction) -> AiResult<OAuthCredential> {
    let device = start_device_authorization().await?;
    interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        interval_seconds: device.interval_seconds,
        expires_in_seconds: device.expires_in_seconds,
    });
    let identity_token = poll_for_identity_token(&device).await?;
    interaction.notify(AuthEvent::Progress {
        message: "Enabling Meta Model API access...".to_string(),
    });
    mint_api_key(&identity_token).await
}

/// The Meta (Muse) OAuth flow.
pub struct MetaOAuth;

impl OAuthFlow for MetaOAuth {
    fn name(&self) -> &str {
        "Meta (Muse subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Meta")
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
        _options: &'a LoginOptions,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move { login(interaction).await })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move { mint_api_key(&credential.refresh).await })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: None,
        })
    }
}
