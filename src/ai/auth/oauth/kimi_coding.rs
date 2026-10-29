//! Kimi Code (subscription) OAuth flow.
//!
//! Rust equivalent of upstream `packages/ai/src/auth/oauth/kimi-coding.ts`:
//! RFC 8628 device authorization against `https://auth.kimi.com` with JSON
//! responses; refresh retries transient failures with backoff; request auth
//! is an `Authorization: Bearer` header.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use super::super::device_code::{poll_device_code_flow, DevicePoll};
use super::super::http::{positive_number, post_form, redact_oauth_body_text, required_string};
use super::super::interaction::{AuthEvent, AuthInteraction};
use super::{now_ms, LoginOptions, OAuthCredential, OAuthFlow};
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::{ModelAuth, ProviderHeaders};

pub const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
pub const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
const REFRESH_MAX_RETRIES: u32 = 3;

/// `KIMI_CODE_OAUTH_HOST` / `KIMI_OAUTH_HOST` override, trailing slashes
/// stripped (upstream `getOauthHost`).
fn oauth_host() -> String {
    let override_host = std::env::var("KIMI_CODE_OAUTH_HOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("KIMI_OAUTH_HOST")
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
    let host = override_host.unwrap_or_else(|| DEFAULT_OAUTH_HOST.to_string());
    host.trim_end_matches('/').to_string()
}

struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    interval_seconds: u64,
    expires_in_seconds: u64,
}

/// The verification URI is opened in the user's browser; only http(s) URLs are
/// trusted.
fn trusted_http_url(value: &str) -> bool {
    (value.starts_with("https://") || value.starts_with("http://"))
        && !value.contains(char::is_whitespace)
}

async fn start_device_authorization(oauth_host: &str) -> AiResult<DeviceAuthorization> {
    let response = post_form(
        &format!("{oauth_host}/api/oauth/device_authorization"),
        &[("client_id", CLIENT_ID)],
    )
    .await?;
    if !response.ok() {
        return Err(AiError::Authentication(format!(
            "Kimi Code device authorization failed with status {}: {}",
            response.status,
            redact_oauth_body_text(&response.body)
        )));
    }
    let json: Value = response.json()?;
    let device_code = required_string(&json, "device_code")?;
    let user_code = required_string(&json, "user_code")?;
    let verification_uri = required_string(&json, "verification_uri")?;
    let verification_uri_complete = required_string(&json, "verification_uri_complete")?;
    if !trusted_http_url(&verification_uri) || !trusted_http_url(&verification_uri_complete) {
        return Err(AiError::Authentication(format!(
            "Invalid Kimi Code device authorization response: {}",
            serde_json::to_string(&json).unwrap_or_default()
        )));
    }
    drop(verification_uri);
    let interval_seconds = json.get("interval").and_then(Value::as_f64);
    let expires_in_seconds = json.get("expires_in").and_then(Value::as_f64);
    Ok(DeviceAuthorization {
        device_code,
        user_code,
        verification_uri_complete,
        interval_seconds: interval_seconds
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| value as u64)
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS),
        expires_in_seconds: expires_in_seconds
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| value as u64)
            .unwrap_or(DEVICE_CODE_TIMEOUT_SECONDS),
    })
}

fn parse_token_response(json: &Value) -> AiResult<OAuthCredential> {
    let access = required_string(json, "access_token")?;
    let refresh = required_string(json, "refresh_token")?;
    let expires_in = positive_number(json, "expires_in")?;
    Ok(OAuthCredential::new(
        access,
        refresh,
        // Provider-controlled `expires_in`: saturate, never overflow.
        now_ms().saturating_add((expires_in * 1000.0) as i64),
    ))
}

async fn poll_for_token(
    oauth_host: &str,
    device: &DeviceAuthorization,
) -> AiResult<OAuthCredential> {
    poll_device_code_flow(
        Some(device.interval_seconds),
        Some(device.expires_in_seconds),
        true,
        || async {
            let response = post_form(
                &format!("{oauth_host}/api/oauth/token"),
                &[
                    ("client_id", CLIENT_ID),
                    ("device_code", &device.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
            )
            .await?;
            if response.status >= 500 {
                return Ok(DevicePoll::Failed {
                    message: format!(
                        "Kimi Code device token request failed with status {}: {}",
                        response.status,
                        redact_oauth_body_text(&response.body)
                    ),
                });
            }
            let json: Value = response.json().unwrap_or(Value::Null);
            if response.ok() && json.get("access_token").and_then(Value::as_str).is_some() {
                return parse_token_response(&json)
                    .map(DevicePoll::Complete)
                    .map_err(|error| AiError::Authentication(error.to_string()));
            }
            let error = json.get("error").and_then(Value::as_str).unwrap_or("");
            match error {
                "authorization_pending" => Ok(DevicePoll::Pending),
                "slow_down" => Ok(DevicePoll::SlowDown {
                    interval_seconds: json
                        .get("interval")
                        .and_then(Value::as_u64)
                        .filter(|value| *value > 0),
                }),
                "expired_token" => Ok(DevicePoll::Failed {
                    message: "Kimi Code device authorization expired. Please restart login."
                        .to_string(),
                }),
                "access_denied" => Ok(DevicePoll::Failed {
                    message: "Kimi Code login was denied.".to_string(),
                }),
                "" => Ok(DevicePoll::Failed {
                    message: format!(
                        "Kimi Code device token request failed (status {})",
                        response.status
                    ),
                }),
                other => {
                    let description = json
                        .get("error_description")
                        .and_then(Value::as_str)
                        .map(|description| format!(": {description}"))
                        .unwrap_or_default();
                    Ok(DevicePoll::Failed {
                        message: format!(
                            "Kimi Code device token request failed (status {}): {other}{description}",
                            response.status
                        ),
                    })
                }
            }
        },
    )
    .await
}

async fn refresh_token(oauth_host: &str, refresh_token: &str) -> AiResult<OAuthCredential> {
    let mut last_error: Option<AiError> = None;
    for attempt in 0..=REFRESH_MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(
                1000 * 2u64.pow(attempt - 1),
            ))
            .await;
        }
        let response = post_form(
            &format!("{oauth_host}/api/oauth/token"),
            &[
                ("client_id", CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ],
        )
        .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let json: Value = response.json().unwrap_or(Value::Null);
        if response.ok() {
            return parse_token_response(&json);
        }
        // Unauthorized: the stored credential is dead; only re-login helps.
        if response.status == 401
            || response.status == 403
            || json.get("error").and_then(Value::as_str) == Some("invalid_grant")
        {
            let description = json
                .get("error_description")
                .and_then(Value::as_str)
                .map(|description| format!(": {description}"))
                .unwrap_or_default();
            return Err(AiError::Authentication(format!(
                "Kimi Code token refresh unauthorized (status {}){description}",
                response.status
            )));
        }
        let retryable = response.status == 429 || response.status >= 500;
        if retryable && attempt < REFRESH_MAX_RETRIES {
            last_error = Some(AiError::Authentication(format!(
                "Kimi Code token refresh failed with status {}",
                response.status
            )));
            continue;
        }
        return Err(AiError::Authentication(format!(
            "Kimi Code token refresh failed with status {}: {}",
            response.status,
            redact_oauth_body_text(&response.body)
        )));
    }
    Err(last_error
        .unwrap_or_else(|| AiError::Authentication("Kimi Code token refresh failed".to_string())))
}

/// The Kimi Code OAuth flow.
pub struct KimiCodingOAuth;

impl OAuthFlow for KimiCodingOAuth {
    fn name(&self) -> &str {
        "Kimi Code (subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Kimi Code")
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
        _options: &'a LoginOptions,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move {
            let host = oauth_host();
            let device = start_device_authorization(&host).await?;
            interaction.notify(AuthEvent::DeviceCode {
                user_code: device.user_code.clone(),
                verification_uri: device.verification_uri_complete.clone(),
                interval_seconds: Some(device.interval_seconds),
                expires_in_seconds: Some(device.expires_in_seconds),
            });
            poll_for_token(&host, &device).await
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move { refresh_token(&oauth_host(), &credential.refresh).await })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        let mut headers = ProviderHeaders::new();
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", credential.access),
        );
        Ok(ModelAuth {
            api_key: None,
            headers: Some(headers),
            base_url: None,
        })
    }
}
