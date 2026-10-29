//! Radius gateway OAuth flow.
//!
//! Rust equivalent of upstream `packages/ai/src/auth/oauth/radius.ts`: a
//! pi-messages gateway whose OAuth client APIs live on the configured gateway
//! (`/v1/oauth` discovery, `/v1/oauth/token`, `/v1/oauth/device`); only the
//! browser authorization endpoint is discovered.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use super::super::callback_server::{CallbackWait, OAuthCallbackServer};
use super::super::device_code::{poll_device_code_flow, DevicePoll};
use super::super::http::{post_form, redact_oauth_body_text};
use super::super::interaction::{AuthEvent, AuthInteraction, AuthPrompt, SelectOption};
use super::super::pkce::generate_pkce;
use super::{now_ms, LoginOptions, OAuthCredential, OAuthFlow};
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::ModelAuth;

pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";
/// Gateway base-URL override (upstream `core/radius.ts`
/// `ENV_RADIUS_GATEWAY = "PI_RADIUS_GATEWAY"`): honored by
/// [`radius_gateway_url`] for every Radius gateway endpoint (OAuth discovery,
/// token/device endpoints, uploads).
pub const ENV_RADIUS_GATEWAY: &str = "PI_RADIUS_GATEWAY";
/// MCP endpoint of the gateway the built-in Radius provider signs in to
/// (upstream `core/radius.ts` `RADIUS_MCP_URL`, computed from the DEFAULT
/// gateway — the `PI_RADIUS_GATEWAY` override does not change this constant
/// upstream either). pir surfaces the constant for parity; wiring it as an
/// MCP server entry (upstream's interactive "Configure Radius MCP" offer)
/// needs the TUI selector surface and is not implemented yet.
pub const RADIUS_MCP_URL: &str = "https://radius.pi.dev/mcp";
const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PORT: u16 = 1456;
const CALLBACK_PATH: &str = "/oauth/callback";
pub const REDIRECT_URI: &str = "http://127.0.0.1:1456/oauth/callback";
const TOKEN_EXPIRY_SKEW_MS: i64 = 60_000;
const LOGIN_METHOD_BROWSER: &str = "browser";
const LOGIN_METHOD_DEVICE_CODE: &str = "device-code";
const OAUTH_CLIENT_ID: &str = "pi-gateway";
const OAUTH_SCOPE: &str = "gateway offline_access";
const OAUTH_DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Normalize a gateway URL: add `https://` when the scheme is missing and
/// strip trailing slashes (upstream `normalizeRadiusGatewayUrl`).
pub fn normalize_radius_gateway_url(value: &str) -> String {
    let with_scheme = if value.starts_with("http://") || value.starts_with("https://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// Radius gateway origin, honoring the `PI_RADIUS_GATEWAY` override
/// (upstream `core/radius.ts getRadiusGatewayUrl`): the trimmed env value
/// normalized, else [`DEFAULT_RADIUS_GATEWAY`].
pub fn radius_gateway_url() -> String {
    match std::env::var(ENV_RADIUS_GATEWAY) {
        Ok(value) if !value.trim().is_empty() => normalize_radius_gateway_url(value.trim()),
        _ => normalize_radius_gateway_url(DEFAULT_RADIUS_GATEWAY),
    }
}

/// Resolve the gateway for a Radius OAuth flow: an explicitly configured
/// gateway wins, otherwise the `PI_RADIUS_GATEWAY` override (see
/// [`radius_gateway_url`]). The default for [`RadiusOAuth::new`] callers that
/// have no per-provider gateway configuration (upstream
/// `experimental/radius-auth.ts` defaults its `gateway` parameter to
/// `getRadiusGatewayUrl()`).
pub fn resolve_radius_gateway(configured: Option<&str>) -> String {
    configured
        .map(normalize_radius_gateway_url)
        .unwrap_or_else(radius_gateway_url)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadiusOAuthError {
    pub status: u16,
    pub oauth_error: Option<String>,
    pub message: String,
}

impl std::fmt::Display for RadiusOAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

/// A failed OAuth token request (upstream `OAuthResponseError`): either a
/// structured non-2xx OAuth response carrying the HTTP status and the `error`
/// code, or a transport/parse failure.
#[derive(Debug)]
pub enum RadiusTokenError {
    OAuth(RadiusOAuthError),
    Transport(AiError),
}

impl std::fmt::Display for RadiusTokenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RadiusTokenError::OAuth(error) => write!(formatter, "{error}"),
            RadiusTokenError::Transport(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<RadiusTokenError> for AiError {
    fn from(error: RadiusTokenError) -> Self {
        match error {
            RadiusTokenError::OAuth(error) => AiError::Authentication(error.message),
            RadiusTokenError::Transport(error) => error,
        }
    }
}

fn oauth_error(status: u16, message: &str, body: &str) -> RadiusOAuthError {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let oauth_error = parsed
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string);
    let description = parsed
        .get("error_description")
        .and_then(Value::as_str)
        .map(str::to_string);
    let detail = match (&oauth_error, &description) {
        (Some(error), Some(description)) => format!("{error}: {description}"),
        (Some(error), None) => error.clone(),
        (None, Some(description)) => description.clone(),
        (None, None) => redact_oauth_body_text(body),
    };
    RadiusOAuthError {
        status,
        oauth_error,
        message: format!("{message}: {detail}"),
    }
}

async fn load_discovery(gateway: &str) -> AiResult<String> {
    let response = super::super::http::get_json(&format!("{gateway}/v1/oauth"), None, &[]).await?;
    if !response.ok() {
        return Err(AiError::Authentication(format!(
            "Could not load Radius OAuth config from {gateway}: {} {}",
            response.status, response.body
        )));
    }
    let json: Value = response.json()?;
    json.get("authorizationEndpoint")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            AiError::Authentication(format!("Invalid Radius OAuth config from {gateway}"))
        })
}

async fn request_oauth_token(
    gateway: &str,
    fields: &[(&str, &str)],
) -> Result<OAuthCredential, RadiusTokenError> {
    let response = post_form(&format!("{gateway}/v1/oauth/token"), fields)
        .await
        .map_err(RadiusTokenError::Transport)?;
    if !response.ok() {
        let error = oauth_error(
            response.status,
            "Radius OAuth token request failed",
            &response.body,
        );
        return Err(RadiusTokenError::OAuth(error));
    }
    let json: Value = response.json().map_err(RadiusTokenError::Transport)?;
    let extract = || -> AiResult<OAuthCredential> {
        let access = json
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AiError::Authentication(
                    "Radius OAuth token response missing access_token".to_string(),
                )
            })?
            .to_string();
        let refresh = json
            .get("refresh_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AiError::Authentication(
                    "Radius OAuth token response missing refresh_token".to_string(),
                )
            })?
            .to_string();
        let expires_in = json
            .get("expires_in")
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                AiError::Authentication(
                    "Radius OAuth token response missing expires_in".to_string(),
                )
            })?;
        let mut credential = OAuthCredential::new(
            access,
            refresh,
            // Provider-controlled `expires_in`: saturate, never overflow.
            now_ms()
                .saturating_add((expires_in * 1000.0) as i64)
                .saturating_sub(TOKEN_EXPIRY_SKEW_MS),
        );
        if let Some(scope) = json.get("scope").and_then(Value::as_str) {
            credential.set_extra("scope", Some(Value::String(scope.to_string())));
        }
        Ok(credential)
    };
    extract().map_err(RadiusTokenError::Transport)
}

/// Map a failed token request onto the device-flow poll outcome by switching
/// on the structured OAuth error code (upstream `switch (error.oauthError)`).
/// `slow_down` keeps the server interval (`None` lets the poller apply the
/// RFC 8628 adjustment).
pub fn device_poll_decision<T>(error: &RadiusTokenError) -> Option<DevicePoll<T>> {
    let RadiusTokenError::OAuth(error) = error else {
        return None;
    };
    let code = error.oauth_error.as_deref()?;
    match code {
        "authorization_pending" => Some(DevicePoll::Pending),
        "slow_down" => Some(DevicePoll::SlowDown {
            interval_seconds: None,
        }),
        "expired_token" => Some(DevicePoll::Failed {
            message: "Device authorization expired.".to_string(),
        }),
        "access_denied" => Some(DevicePoll::Failed {
            message: "Device authorization was denied.".to_string(),
        }),
        _ => None,
    }
}

async fn login_with_browser(
    gateway: &str,
    authorization_endpoint: &str,
    interaction: &dyn AuthInteraction,
) -> AiResult<OAuthCredential> {
    let pkce = generate_pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let authorize_url = format!(
        "{authorization_endpoint}?response_type=code&client_id={OAUTH_CLIENT_ID}&redirect_uri={}\
         &scope={}&code_challenge={}&code_challenge_method=S256&handoff=url&state={state}",
        super::anthropic::urlencode(REDIRECT_URI),
        super::anthropic::urlencode(OAUTH_SCOPE),
        pkce.challenge,
    );

    let callback = OAuthCallbackServer::start(
        "Radius",
        CALLBACK_HOST,
        CALLBACK_PORT,
        CALLBACK_PATH,
        None,
        Some(&state),
    )
    .await?;
    interaction.notify(AuthEvent::Progress {
        message: format!("Listening for OAuth callback on {REDIRECT_URI}"),
    });
    interaction.notify(AuthEvent::AuthUrl {
        url: authorize_url,
        instructions: Some("Continue in your browser.".to_string()),
    });

    let result = async {
        let code = match callback.wait(None).await? {
            CallbackWait::Completed(pairs) => {
                // A provider-reported `?error=` callback settles the wait
                // with the failure (upstream `finish({error})` rejects).
                if let Some(error) = super::super::callback_server::authorization_error(&pairs) {
                    return Err(AiError::Authentication(format!(
                        "Radius authorization failed: {error}"
                    )));
                }
                super::super::callback_server::query_param(&pairs, "code")
                    .ok_or_else(|| {
                        AiError::Authentication("Missing authorization code".to_string())
                    })?
                    .to_string()
            }
            CallbackWait::Cancelled => {
                return Err(AiError::Authentication(
                    "OAuth callback did not complete.".to_string(),
                ))
            }
        };
        request_oauth_token(
            gateway,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", OAUTH_CLIENT_ID),
                ("redirect_uri", REDIRECT_URI),
                ("code", &code),
                ("code_verifier", &pkce.verifier),
            ],
        )
        .await
        .map_err(AiError::from)
    }
    .await;
    // Answer the browser page with the real exchange outcome (upstream sends
    // the page only after `complete` settles): success renders 200, a failure
    // renders 502 carrying the error.
    callback.confirm_page(result.as_ref().err().map(|error| error.to_string()));
    callback.close();
    result
}

async fn request_device_authorization(
    gateway: &str,
) -> AiResult<(String, String, String, u64, Option<u64>)> {
    let response = post_form(
        &format!("{gateway}/v1/oauth/device"),
        &[("client_id", OAUTH_CLIENT_ID), ("scope", OAUTH_SCOPE)],
    )
    .await?;
    if !response.ok() {
        let error = oauth_error(
            response.status,
            "Radius OAuth device authorization failed",
            &response.body,
        );
        return Err(AiError::Authentication(error.to_string()));
    }
    let json: Value = response.json()?;
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
        .get("verification_uri")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let expires_in = json
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0);
    let interval = json.get("interval").and_then(Value::as_u64);
    match (device_code, user_code, verification_uri, expires_in) {
        (Some(device_code), Some(user_code), Some(verification_uri), Some(expires_in)) => Ok((
            device_code,
            user_code,
            verification_uri,
            expires_in,
            interval,
        )),
        _ => Err(AiError::Authentication(
            "Radius OAuth device authorization response is missing required fields".to_string(),
        )),
    }
}

async fn login_with_device_code(
    gateway: &str,
    interaction: &dyn AuthInteraction,
) -> AiResult<OAuthCredential> {
    let (device_code, user_code, verification_uri, expires_in, interval) =
        request_device_authorization(gateway).await?;
    interaction.notify(AuthEvent::DeviceCode {
        user_code,
        verification_uri,
        interval_seconds: interval,
        expires_in_seconds: Some(expires_in),
    });
    poll_device_code_flow(interval, Some(expires_in), false, || async {
        match request_oauth_token(
            gateway,
            &[
                ("grant_type", OAUTH_DEVICE_CODE_GRANT_TYPE),
                ("client_id", OAUTH_CLIENT_ID),
                ("device_code", &device_code),
            ],
        )
        .await
        {
            Ok(credential) => Ok(DevicePoll::Complete(credential)),
            Err(error) => match device_poll_decision(&error) {
                Some(poll) => Ok(poll),
                None => Err(AiError::from(error)),
            },
        }
    })
    .await
}

/// The Radius gateway OAuth flow, bound to one gateway URL.
pub struct RadiusOAuth {
    display_name: String,
    gateway: String,
}

impl RadiusOAuth {
    /// Create the flow for a gateway URL (upstream `createRadiusOAuth`).
    pub fn new(display_name: impl Into<String>, gateway: &str) -> Self {
        Self {
            display_name: display_name.into(),
            gateway: normalize_radius_gateway_url(gateway),
        }
    }
}

impl OAuthFlow for RadiusOAuth {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
        _options: &'a LoginOptions,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move {
            let login_method = interaction
                .prompt(AuthPrompt::Select {
                    message: format!("Sign in to {}:", self.display_name),
                    options: vec![
                        SelectOption {
                            id: LOGIN_METHOD_BROWSER.to_string(),
                            label: "Sign in with browser (recommended)".to_string(),
                            description: None,
                        },
                        SelectOption {
                            id: LOGIN_METHOD_DEVICE_CODE.to_string(),
                            label: "Sign in with device code (when signing in from another device)"
                                .to_string(),
                            description: None,
                        },
                    ],
                })
                .await?;
            match login_method.as_str() {
                LOGIN_METHOD_DEVICE_CODE => {
                    login_with_device_code(&self.gateway, interaction).await
                }
                LOGIN_METHOD_BROWSER => {
                    let discovery = load_discovery(&self.gateway).await?;
                    login_with_browser(&self.gateway, &discovery, interaction).await
                }
                other => Err(AiError::Authentication(format!(
                    "Unknown {} sign-in method: {other}",
                    self.display_name
                ))),
            }
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move {
            request_oauth_token(
                &self.gateway,
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", OAUTH_CLIENT_ID),
                    ("refresh_token", &credential.refresh),
                ],
            )
            .await
            .map_err(AiError::from)
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: None,
        })
    }
}
