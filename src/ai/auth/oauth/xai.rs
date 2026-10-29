//! xAI OAuth device-code flow.
//!
//! Rust equivalent of upstream `packages/ai/src/auth/oauth/xai.ts`.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use super::super::device_code::{poll_device_code_flow, DevicePoll};
use super::super::http::{oauth_http_failure, positive_number, post_form, required_string};
use super::super::interaction::{AuthEvent, AuthInteraction};
use super::{now_ms, LoginOptions, OAuthCredential, OAuthFlow};
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::ModelAuth;

const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const XAI_DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const XAI_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
/// Refresh slightly before the reported expiry to avoid using a token that
/// dies mid-request.
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
const DEFAULT_TOKEN_LIFETIME_SECONDS: f64 = 3600.0;

/// The verification URI is opened in the user's browser; force it to be an
/// https URL so a malicious response cannot launch something else.
pub fn validate_verification_uri(raw: &str) -> AiResult<String> {
    if raw.starts_with("https://") && !raw.contains(char::is_whitespace) {
        Ok(raw.to_string())
    } else {
        Err(AiError::Authentication(
            "Untrusted verification URI in xAI OAuth response".to_string(),
        ))
    }
}

fn credentials_from_token_response(
    body: &Value,
    previous_refresh_token: Option<&str>,
) -> AiResult<OAuthCredential> {
    let access = required_string(body, "access_token")?;
    // xAI may omit refresh_token on refresh when the token is not rotated.
    let refresh = match (body.get("refresh_token"), previous_refresh_token) {
        (None, Some(previous)) => previous.to_string(),
        _ => required_string(body, "refresh_token")?,
    };
    let expires_in_seconds = match body.get("expires_in") {
        None => DEFAULT_TOKEN_LIFETIME_SECONDS,
        Some(_) => positive_number(body, "expires_in")?,
    };
    Ok(OAuthCredential::new(
        access,
        refresh,
        // Provider-controlled `expires_in`: saturate, never overflow.
        now_ms()
            .saturating_add((expires_in_seconds * 1000.0) as i64)
            .saturating_sub(REFRESH_SKEW_MS),
    ))
}

async fn post_form_checked(url: &str, fields: &[(&str, &str)]) -> AiResult<(bool, u16, Value)> {
    let response = post_form(url, fields).await?;
    let body: Value = response.json().unwrap_or(Value::Null);
    Ok((response.ok(), response.status, body))
}

struct XaiDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    interval_seconds: Option<u64>,
    expires_in_seconds: u64,
}

async fn request_device_code() -> AiResult<XaiDeviceCode> {
    let (ok, status, body) = post_form_checked(
        XAI_DEVICE_CODE_URL,
        &[
            ("client_id", XAI_CLIENT_ID),
            ("scope", XAI_SCOPE),
            ("referrer", "pi"),
        ],
    )
    .await?;
    if !ok {
        return Err(oauth_http_failure("device authorization", status, &body));
    }
    let device_code = required_string(&body, "device_code")?;
    let user_code = required_string(&body, "user_code")?;
    let verification_uri = validate_verification_uri(&required_string(&body, "verification_uri")?)?;
    let verification_uri_complete = match body
        .get("verification_uri_complete")
        .and_then(Value::as_str)
    {
        Some(value) if !value.is_empty() => Some(validate_verification_uri(value)?),
        _ => None,
    };
    let interval_seconds = body
        .get("interval")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value as u64);
    let expires_in_seconds = positive_number(&body, "expires_in")? as u64;
    Ok(XaiDeviceCode {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete,
        interval_seconds,
        expires_in_seconds,
    })
}

async fn login(interaction: &dyn AuthInteraction) -> AiResult<OAuthCredential> {
    let device = request_device_code().await?;
    interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| device.verification_uri.clone()),
        interval_seconds: device.interval_seconds,
        expires_in_seconds: Some(device.expires_in_seconds),
    });
    poll_device_code_flow(
        device.interval_seconds,
        Some(device.expires_in_seconds),
        true,
        || async {
            let (ok, status, body) = post_form_checked(
                XAI_TOKEN_URL,
                &[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", XAI_CLIENT_ID),
                    ("device_code", &device.device_code),
                ],
            )
            .await?;
            if ok {
                return credentials_from_token_response(&body, None).map(DevicePoll::Complete);
            }
            let error = body.get("error").and_then(Value::as_str).unwrap_or("");
            match error {
                "authorization_pending" => Ok(DevicePoll::Pending),
                "slow_down" => Ok(DevicePoll::SlowDown {
                    interval_seconds: body.get("interval").and_then(Value::as_u64),
                }),
                "access_denied" | "authorization_denied" => Ok(DevicePoll::Failed {
                    message: "xAI device authorization was denied".to_string(),
                }),
                "expired_token" => Ok(DevicePoll::Failed {
                    message: "xAI device code expired".to_string(),
                }),
                _ => Ok(DevicePoll::Failed {
                    message: oauth_http_failure("device token polling", status, &body).to_string(),
                }),
            }
        },
    )
    .await
}

async fn refresh_xai_token(refresh_token: &str) -> AiResult<OAuthCredential> {
    let (ok, status, body) = post_form_checked(
        XAI_TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", XAI_CLIENT_ID),
            ("refresh_token", refresh_token),
        ],
    )
    .await?;
    if !ok {
        return Err(oauth_http_failure("token refresh", status, &body));
    }
    credentials_from_token_response(&body, Some(refresh_token))
}

/// The xAI OAuth flow.
pub struct XaiOAuth;

impl OAuthFlow for XaiOAuth {
    fn name(&self) -> &str {
        "xAI (Grok/X subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with SuperGrok or X Premium")
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
        Box::pin(async move { refresh_xai_token(&credential.refresh).await })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: None,
        })
    }
}
