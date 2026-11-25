//! OpenAI Responses API token sharing through Sign in with ChatGPT.
//!
//! Rust equivalent of upstream `packages/ai/src/auth/oauth/openai-chatgpt.ts`:
//! a public-client flow with no client secret. Every login registers a new
//! client (`dynamic_agent_client`); OpenAI returns the issued client id in the
//! callback, which is stored on the credential and reused for refresh.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use super::super::callback_server::{CallbackWait, OAuthCallbackServer};
use super::super::http::{post_form, redact_oauth_body_text};
use super::super::interaction::{AuthEvent, AuthInteraction, AuthPrompt};
use super::super::pkce::generate_pkce;
use super::{now_ms, LoginOptions, OAuthCredential, OAuthFlow};
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::ModelAuth;

/// Every login registers a new client with this id; OpenAI returns the issued
/// client id in the callback.
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
const AGENT_NAME_HINT: &str = "Pi";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const RESOURCE: &str = "https://api.openai.com/v1";
pub const CALLBACK_PORT: u16 = 1455;
pub const CALLBACK_PATH: &str = "/auth/callback";
/// The default redirect URI (callback host `127.0.0.1`); the active redirect
/// URI is built from `PI_OAUTH_CALLBACK_HOST` at login time.
pub const REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";
const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
pub const SCOPE: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// Refresh long before the real expiry so a request never starts with a token
/// about to expire.
const EXPIRY_MARGIN_MS: i64 = 3 * 60 * 1000;

/// The redirect URI for the configured callback host
/// (`PI_OAUTH_CALLBACK_HOST`, default `127.0.0.1`).
pub fn redirect_uri() -> String {
    format!(
        "http://{}:{CALLBACK_PORT}{CALLBACK_PATH}",
        super::callback_host()
    )
}

pub struct AuthorizationResult {
    pub code: String,
    pub client_id: String,
}

fn random_value() -> String {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    super::super::pkce::base64url_encode(&bytes)
}

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        match index {
            8 | 13 | 18 | 23 => {
                if *byte != b'-' {
                    return false;
                }
            }
            _ => {
                if !byte.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

/// OpenAI identifies each installation ("agent host") by a stable URI such as
/// `urn:uuid:<uuid>` (upstream `agentHostId`).
pub fn agent_host_id(device_id: &str) -> AiResult<String> {
    if !is_uuid(device_id) {
        return Err(AiError::Authentication(
            "Sign in with ChatGPT requires a device ID (UUID) for this installation".to_string(),
        ));
    }
    Ok(format!("urn:uuid:{}", device_id.to_lowercase()))
}

/// Build the authorization result from a callback query
/// (upstream `authorizationResultFromCallback`): the code, the state, and —
/// required — the issued client id OpenAI echoes on the callback.
pub fn result_from_callback_query(
    pairs: &[(String, String)],
    expected_state: &str,
) -> AiResult<AuthorizationResult> {
    // A provider-reported `?error=` callback settles the wait with the
    // failure (upstream `finish({error})` rejects).
    if let Some(error) = super::super::callback_server::authorization_error(pairs) {
        return Err(AiError::Authentication(format!(
            "ChatGPT authorization failed: {error}"
        )));
    }
    let code = super::super::callback_server::query_param(pairs, "code")
        .ok_or_else(|| AiError::Authentication("Missing authorization code".to_string()))?
        .to_string();
    let state = super::super::callback_server::query_param(pairs, "state")
        .ok_or_else(|| AiError::Authentication("Missing OAuth state".to_string()))?;
    if state != expected_state {
        return Err(AiError::Authentication("OAuth state mismatch".to_string()));
    }
    let client_id = super::super::callback_server::query_param(pairs, "client_id")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AiError::Authentication(
                "OpenAI OAuth registration callback did not contain an issued client ID"
                    .to_string(),
            )
        })?
        .to_string();
    Ok(AuthorizationResult { code, client_id })
}

/// Parse the manually pasted callback URL against a redirect URI.
fn result_from_manual_input_with_redirect(
    input: &str,
    expected_state: &str,
    redirect_uri: &str,
) -> AiResult<AuthorizationResult> {
    let value = input.trim();
    let (origin, path, query) = match value.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, rest) = rest.split_once('/').unwrap_or((rest, ""));
            let (path, query) = match rest.split_once('?') {
                Some((path, query)) => (format!("/{path}"), query),
                None => (format!("/{rest}"), ""),
            };
            (format!("{scheme}://{authority}"), path, query)
        }
        None => {
            return Err(AiError::Authentication(
                "Paste the full callback URL from the browser".to_string(),
            ))
        }
    };
    let (expected_origin, expected_path) = match redirect_uri.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = match rest.split_once('/') {
                Some((authority, path)) => (authority, format!("/{path}")),
                None => (rest, "/".to_string()),
            };
            (format!("{scheme}://{authority}"), path)
        }
        None => (redirect_uri.to_string(), "/".to_string()),
    };
    if origin != expected_origin || path != expected_path {
        return Err(AiError::Authentication(format!(
            "The pasted callback URL must start with {redirect_uri}"
        )));
    }
    let pairs = super::super::callback_server::parse_query(query);
    if let Some(error) = super::super::callback_server::query_param(&pairs, "error") {
        return Err(AiError::Authentication(format!(
            "ChatGPT authorization failed: {error}"
        )));
    }
    result_from_callback_query(&pairs, expected_state)
}

/// Parse the manually pasted callback URL (default redirect URI).
pub fn result_from_manual_input(
    input: &str,
    expected_state: &str,
) -> AiResult<AuthorizationResult> {
    result_from_manual_input_with_redirect(input, expected_state, REDIRECT_URI)
}

/// Build the stored OAuth credential from a token response. Public so the
/// callback-to-credential contract is testable end to end.
pub fn credential_from_token_response(token: &Value, client_id: &str) -> AiResult<OAuthCredential> {
    let access = token
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AiError::Authentication(
                "OpenAI OAuth token response has invalid access_token".to_string(),
            )
        })?;
    let refresh = token
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AiError::Authentication(
                "OpenAI OAuth token response has invalid refresh_token".to_string(),
            )
        })?;
    let scope = token
        .get("scope")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AiError::Authentication("OpenAI OAuth token response has invalid scope".to_string())
        })?;
    let expires_in = token
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| {
            AiError::Authentication(
                "OpenAI OAuth token response has invalid expires_in".to_string(),
            )
        })?;
    let scopes: Vec<Value> = scope
        .split_whitespace()
        .map(str::to_string)
        .map(Value::String)
        .collect();
    if !scopes.iter().any(|entry| entry == DIRECT_TOKEN_SCOPE) {
        return Err(AiError::Authentication(format!(
            "OpenAI OAuth grant did not include {DIRECT_TOKEN_SCOPE}"
        )));
    }
    let mut credential = OAuthCredential::new(
        access,
        refresh,
        // Provider-controlled `expires_in`; saturate instead of panicking
        // (debug) or wrapping to a past expiry (release).
        now_ms()
            .saturating_add((expires_in * 1000.0) as i64)
            .saturating_sub(EXPIRY_MARGIN_MS),
    );
    credential.set_extra("clientId", Some(Value::String(client_id.to_string())));
    credential.set_extra("scopes", Some(Value::Array(scopes)));
    Ok(credential)
}

async fn request_token(fields: &[(&str, &str)]) -> AiResult<Value> {
    let response = post_form(TOKEN_URL, fields).await?;
    if !response.ok() {
        return Err(AiError::Authentication(format!(
            "OpenAI OAuth token request failed ({}): {}",
            response.status,
            redact_oauth_body_text(&response.body)
        )));
    }
    response.json()
}

async fn exchange_authorization_code(
    code: &str,
    verifier: &str,
    client_id: &str,
    redirect_uri: &str,
) -> AiResult<OAuthCredential> {
    let token = request_token(&[
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
        ("resource", RESOURCE),
    ])
    .await?;
    // Pi does not use the ID token to identify the user; keep the presence
    // check as part of the token-response contract.
    if token
        .get("id_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return Err(AiError::Authentication(
            "OpenAI OAuth token response did not contain an ID token".to_string(),
        ));
    }
    credential_from_token_response(&token, client_id)
}

async fn refresh_access_token(credential: &OAuthCredential) -> AiResult<OAuthCredential> {
    let Some(client_id) = credential.extra_str("clientId").map(str::to_string) else {
        return Err(AiError::Authentication(
            "Stored OpenAI OAuth credential does not contain an issued client ID; reconnect ChatGPT"
                .to_string(),
        ));
    };
    if client_id.trim().is_empty() {
        return Err(AiError::Authentication(
            "Stored OpenAI OAuth credential does not contain an issued client ID; reconnect ChatGPT"
                .to_string(),
        ));
    }
    let token = request_token(&[
        ("grant_type", "refresh_token"),
        ("client_id", &client_id),
        ("refresh_token", &credential.refresh),
        ("resource", RESOURCE),
    ])
    .await?;
    credential_from_token_response(&token, &client_id)
}

async fn login(
    interaction: &dyn AuthInteraction,
    options: &LoginOptions,
) -> AiResult<OAuthCredential> {
    let device_id = options.device_id()?;
    let host_id = agent_host_id(&device_id)?;
    let agent_name = options
        .agent_name
        .clone()
        .unwrap_or_else(|| AGENT_NAME_HINT.to_string());
    let pkce = generate_pkce();
    let state = random_value();
    let redirect_uri = redirect_uri();
    let callback_host = super::callback_host();
    // Without this server, the browser's callback would reach whatever else
    // holds the port, which rejects it as a state mismatch.
    let callback = OAuthCallbackServer::start(
        "OpenAI",
        &callback_host,
        CALLBACK_PORT,
        CALLBACK_PATH,
        None,
        Some(&state),
    )
    .await
    .map_err(|error| {
        AiError::Authentication(format!(
            "Port {CALLBACK_PORT} is in use, probably by an unfinished login in another pi session \
             or by the Codex CLI. Cancel that login and try again. ({error})"
        ))
    })?;

    let authorize_url = format!(
        "{AUTHORIZE_URL}?client_id={DYNAMIC_CLIENT_ID}&agent_name_hint={}&ext_agent_host_id={}\
         &response_type=code&redirect_uri={}&resource={}&scope={}&state={state}\
         &code_challenge={}&code_challenge_method=S256&nonce={}",
        super::anthropic::urlencode(&agent_name),
        super::anthropic::urlencode(&host_id),
        super::anthropic::urlencode(&redirect_uri),
        super::anthropic::urlencode(RESOURCE),
        super::anthropic::urlencode(SCOPE),
        pkce.challenge,
        random_value(),
    );
    interaction.notify(AuthEvent::AuthUrl {
        url: authorize_url,
        instructions: Some(
            "Complete sign-in in your browser. If the callback does not complete, paste the final \
             redirect URL here."
                .to_string(),
        ),
    });

    let result: AiResult<OAuthCredential> = async {
        let authorization = tokio::select! {
            wait = callback.wait(None) => match wait? {
                // The full callback query — including the issued client_id —
                // travels through the channel (upstream
                // `authorizationResultFromCallback` requires it).
                CallbackWait::Completed(pairs) => result_from_callback_query(&pairs, &state)?,
                CallbackWait::Cancelled => {
                    let input = interaction
                        .prompt(AuthPrompt::ManualCode {
                            message: "Complete login in your browser, or paste the final redirect URL here:"
                                .to_string(),
                            placeholder: Some(redirect_uri.clone()),
                        })
                        .await?;
                    result_from_manual_input_with_redirect(&input, &state, &redirect_uri)?
                }
            },
            manual = interaction.prompt(AuthPrompt::ManualCode {
                message: "Complete login in your browser, or paste the final redirect URL here:".to_string(),
                placeholder: Some(redirect_uri.clone()),
            }) => {
                callback.cancel();
                result_from_manual_input_with_redirect(&manual?, &state, &redirect_uri)?
            }
        };
        interaction.notify(AuthEvent::Progress {
            message: "Exchanging authorization code for tokens...".to_string(),
        });
        exchange_authorization_code(
            &authorization.code,
            &pkce.verifier,
            &authorization.client_id,
            &redirect_uri,
        )
        .await
    }
    .await;
    // Answer the browser page with the real exchange outcome (upstream sends
    // the page only after `complete` settles): success renders 200, a failure
    // renders 502 carrying the error.
    callback.confirm_page(result.as_ref().err().map(|error| error.to_string()));
    callback.close();
    result
}

/// The OpenAI ChatGPT subscription OAuth flow.
pub struct OpenAiChatGptOAuth;

impl OAuthFlow for OpenAiChatGptOAuth {
    fn name(&self) -> &str {
        "OpenAI (ChatGPT subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with ChatGPT")
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
        options: &'a LoginOptions,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move { login(interaction, options).await })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move { refresh_access_token(credential).await })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: None,
        })
    }
}
