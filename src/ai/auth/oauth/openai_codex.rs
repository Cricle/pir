//! OpenAI Codex (ChatGPT OAuth) flow.
//!
//! Rust equivalent of upstream `packages/ai/src/auth/oauth/openai-codex.ts`:
//! browser PKCE login on port 1455 plus the headless device-code variant, and
//! the `https://api.openai.com/auth.chatgpt_account_id` extraction.

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use super::super::callback_server::OAuthCallbackServer;
use super::super::device_code::{poll_device_code_flow, DevicePoll};
use super::super::http::{post_form, post_json, redact_oauth_body_text, required_string};
use super::super::interaction::{AuthEvent, AuthInteraction, AuthPrompt, SelectOption};
use super::super::pkce::generate_pkce;
use super::{decode_jwt_payload, now_ms, LoginOptions, OAuthCredential, OAuthFlow};
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::ModelAuth;

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
pub const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
pub const BROWSER_LOGIN_METHOD: &str = "browser";
const DEVICE_CODE_LOGIN_METHOD: &str = "device_code";
pub const SCOPE: &str = "openid profile email offline_access";
pub const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

const BROWSER_CALLBACK_PORT: u16 = 1455;
const BROWSER_CALLBACK_PATH: &str = "/auth/callback";

/// A token endpoint response reduced to the three fields the flow keeps.
struct OAuthToken {
    access: String,
    refresh: String,
    expires: i64,
}

fn random_state() -> String {
    // 16 random bytes hex, derived from two UUIDv4 halves like the PKCE RNG.
    let first = *uuid::Uuid::new_v4().as_bytes();
    let second = *uuid::Uuid::new_v4().as_bytes();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&first[..8]);
    bytes[8..].copy_from_slice(&second[..8]);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_token_response(status: u16, body: &str, operation: &str) -> AiResult<OAuthToken> {
    if !(200..300).contains(&status) {
        return Err(AiError::Authentication(format!(
            "OpenAI Codex token {operation} failed ({status}): {}",
            redact_oauth_body_text(body)
        )));
    }
    let json: Value = serde_json::from_str(body).map_err(|error| {
        AiError::Authentication(format!(
            "OpenAI Codex token {operation} response missing fields: invalid JSON ({error}): {}",
            redact_oauth_body_text(body)
        ))
    })?;
    let access = required_string(&json, "access_token")?;
    let refresh = required_string(&json, "refresh_token")?;
    let expires_in = super::super::http::positive_number(&json, "expires_in")?;
    Ok(OAuthToken {
        access,
        refresh,
        // Provider-controlled `expires_in`: saturate, never overflow.
        expires: now_ms().saturating_add((expires_in * 1000.0) as i64),
    })
}

async fn exchange_authorization_code(
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> AiResult<OAuthToken> {
    // The token endpoint is form-encoded (upstream openai-codex.ts:151-179;
    // the in-repo ChatGPT flow's `request_token` shows the same shape).
    let response = post_form(
        TOKEN_URL,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ],
    )
    .await?;
    read_token_response(response.status, &response.body, "exchange")
}

async fn refresh_access_token(refresh_token: &str) -> AiResult<OAuthToken> {
    let response = post_form(
        TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ],
    )
    .await
    .map_err(|error| {
        AiError::Authentication(format!("OpenAI Codex token refresh error: {error}"))
    })?;
    read_token_response(response.status, &response.body, "refresh")
}

struct DeviceAuthInfo {
    device_auth_id: String,
    user_code: String,
    interval_seconds: u64,
}

async fn start_device_auth() -> AiResult<DeviceAuthInfo> {
    let response = post_json(DEVICE_USER_CODE_URL, &json!({ "client_id": CLIENT_ID })).await?;
    if !response.ok() {
        if response.status == 404 {
            return Err(AiError::Authentication(
                "OpenAI Codex device code login is not enabled for this server. Use browser login \
                 or verify the server URL."
                    .to_string(),
            ));
        }
        return Err(AiError::Authentication(format!(
            "OpenAI Codex device code request failed with status {}{}",
            response.status,
            if response.body.is_empty() {
                String::new()
            } else {
                format!(": {}", redact_oauth_body_text(&response.body))
            }
        )));
    }
    let json: Value = response.json()?;
    let device_auth_id = required_string(&json, "device_auth_id")?;
    let user_code = required_string(&json, "user_code")?;
    let interval_seconds = match json.get("interval") {
        Some(Value::String(text)) => text.trim().parse::<u64>().ok(),
        Some(Value::Number(number)) => number.as_u64(),
        _ => None,
    }
    .ok_or_else(|| {
        AiError::Authentication(format!(
            "Invalid OpenAI Codex device code response: {}",
            redact_oauth_body_text(&response.body)
        ))
    })?;
    Ok(DeviceAuthInfo {
        device_auth_id,
        user_code,
        interval_seconds,
    })
}

struct DeviceTokenSuccess {
    authorization_code: String,
    code_verifier: String,
}

async fn poll_device_auth(device: &DeviceAuthInfo) -> AiResult<DeviceTokenSuccess> {
    poll_device_code_flow(
        Some(device.interval_seconds),
        Some(DEVICE_CODE_TIMEOUT_SECONDS),
        false,
        || async {
            let response = post_json(
                DEVICE_TOKEN_URL,
                &json!({
                    "device_auth_id": device.device_auth_id,
                    "user_code": device.user_code,
                }),
            )
            .await?;
            if response.ok() {
                let json: Value = response.json()?;
                let authorization_code = required_string(&json, "authorization_code")?;
                let code_verifier = required_string(&json, "code_verifier")?;
                return Ok(DevicePoll::Complete(DeviceTokenSuccess {
                    authorization_code,
                    code_verifier,
                }));
            }
            if response.status == 403 || response.status == 404 {
                return Ok(DevicePoll::Pending);
            }
            let error_code = response
                .json()
                .ok()
                .and_then(|json| json.get("error").cloned())
                .map(|error| match error {
                    Value::String(text) => text,
                    Value::Object(map) => map
                        .get("code")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_default(),
                    _ => String::new(),
                })
                .unwrap_or_default();
            if error_code == "deviceauth_authorization_pending" {
                return Ok(DevicePoll::Pending);
            }
            if error_code == "slow_down" {
                return Ok(DevicePoll::SlowDown {
                    interval_seconds: None,
                });
            }
            Ok(DevicePoll::Failed {
                message: format!(
                    "OpenAI Codex device auth failed with status {}{}",
                    response.status,
                    if response.body.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", redact_oauth_body_text(&response.body))
                    }
                ),
            })
        },
    )
    .await
}

/// Extract the ChatGPT account id from the access token's JWT.
fn account_id_from_access(access: &str) -> Option<String> {
    let payload = decode_jwt_payload(access)?;
    let auth = payload.get(JWT_CLAIM_PATH)?;
    auth.get("chatgpt_account_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn credentials_from_token(token: OAuthToken) -> AiResult<OAuthCredential> {
    let account_id = account_id_from_access(&token.access).ok_or_else(|| {
        AiError::Authentication("Failed to extract accountId from token".to_string())
    })?;
    let mut credential = OAuthCredential::new(token.access, token.refresh, token.expires);
    credential.set_extra("accountId", Some(Value::String(account_id)));
    Ok(credential)
}

async fn login_device_code(interaction: &dyn AuthInteraction) -> AiResult<OAuthCredential> {
    let device = start_device_auth().await?;
    interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: DEVICE_VERIFICATION_URI.to_string(),
        interval_seconds: Some(device.interval_seconds),
        expires_in_seconds: Some(DEVICE_CODE_TIMEOUT_SECONDS),
    });
    let code = poll_device_auth(&device).await?;
    credentials_from_token(
        exchange_authorization_code(
            &code.authorization_code,
            &code.code_verifier,
            DEVICE_REDIRECT_URI,
        )
        .await?,
    )
}

async fn login_browser(
    interaction: &dyn AuthInteraction,
    options: &LoginOptions,
) -> AiResult<OAuthCredential> {
    let pkce = generate_pkce();
    let state = random_state();
    let originator = options
        .agent_name
        .clone()
        .unwrap_or_else(|| "pi".to_string());
    // Port 1455 is shared with the Codex CLI; when it is taken, fall back to
    // the pasted redirect URL.
    let callback = OAuthCallbackServer::start(
        "OpenAI",
        &super::callback_host(),
        BROWSER_CALLBACK_PORT,
        BROWSER_CALLBACK_PATH,
        None,
        Some(&state),
    )
    .await
    .ok();
    let redirect_uri = callback
        .as_ref()
        .map(|callback| callback.redirect_uri().to_string())
        .unwrap_or_else(|| REDIRECT_URI.to_string());

    let result: AiResult<OAuthCredential> = async {
        let authorize_url = format!(
            "{AUTHORIZE_URL}?response_type=code&client_id={CLIENT_ID}&redirect_uri={}&scope={}\
             &code_challenge={}&code_challenge_method=S256&state={state}\
             &id_token_add_organizations=true&codex_cli_simplified_flow=true&originator={originator}",
            super::anthropic::urlencode(&redirect_uri),
            super::anthropic::urlencode(SCOPE),
            pkce.challenge,
        );
        interaction.notify(AuthEvent::AuthUrl {
            url: authorize_url,
            instructions: Some("A browser window should open. Complete login to finish.".to_string()),
        });

        let (code, _state_in) = match &callback {
            Some(callback) => {
                super::anthropic::wait_callback_or_manual(
                    callback,
                    interaction,
                    AuthPrompt::ManualCode {
                        message:
                            "Complete login in your browser, or paste the authorization code / redirect URL here:"
                                .to_string(),
                        placeholder: Some(REDIRECT_URI.to_string()),
                    },
                    &state,
                )
                .await?
            }
            None => {
                let input = interaction
                    .prompt(AuthPrompt::ManualCode {
                        message:
                            "Complete login in your browser, or paste the authorization code / redirect URL here:"
                                .to_string(),
                        placeholder: Some(REDIRECT_URI.to_string()),
                    })
                    .await?;
                super::anthropic::parse_code_input(&input, &state)?
            }
        };
        // The callback server already validated the state; manual input is
        // validated against it above (a callback delivery carries no echoable
        // state here, mirroring upstream where only `code` is used).
        let Some(code) = code else {
            return Err(AiError::Authentication("Missing authorization code".to_string()));
        };
        credentials_from_token(exchange_authorization_code(&code, &pkce.verifier, &redirect_uri).await?)
    }
    .await;
    // Answer the browser page with the real exchange outcome (upstream sends
    // the page only after `complete` settles): success renders 200, a failure
    // renders 502 carrying the error.
    if let Some(callback) = &callback {
        callback.confirm_page(result.as_ref().err().map(|error| error.to_string()));
        callback.close();
    }
    result
}

/// The OpenAI Codex OAuth flow.
pub struct OpenAiCodexOAuth;

impl OAuthFlow for OpenAiCodexOAuth {
    fn name(&self) -> &str {
        "OpenAI (ChatGPT Plus/Pro)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
        options: &'a LoginOptions,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move {
            let method = interaction
                .prompt(AuthPrompt::Select {
                    message: "Select OpenAI Codex login method:".to_string(),
                    options: vec![
                        SelectOption {
                            id: BROWSER_LOGIN_METHOD.to_string(),
                            label: "Browser login (default)".to_string(),
                            description: None,
                        },
                        SelectOption {
                            id: DEVICE_CODE_LOGIN_METHOD.to_string(),
                            label: "Device code login (headless)".to_string(),
                            description: None,
                        },
                    ],
                })
                .await?;
            match method.as_str() {
                DEVICE_CODE_LOGIN_METHOD => login_device_code(interaction).await,
                BROWSER_LOGIN_METHOD => login_browser(interaction, options).await,
                other => Err(AiError::Authentication(format!(
                    "Unknown OpenAI Codex login method: {other}"
                ))),
            }
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(
            async move { credentials_from_token(refresh_access_token(&credential.refresh).await?) },
        )
    }

    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: None,
        })
    }
}
