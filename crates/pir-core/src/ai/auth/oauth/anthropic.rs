//! Anthropic OAuth flow (Claude Pro/Max).
//!
//! Rust equivalent of upstream `packages/ai/src/auth/oauth/anthropic.ts`.
//! Constants are copied verbatim (the upstream client id is a base64 literal
//! decoding to `9d1c250a-e61b-44d9-88ed-5944d1962f5e`).

use std::future::Future;
use std::pin::Pin;

use serde_json::json;

use super::super::callback_server::{CallbackWait, OAuthCallbackServer};
use super::super::http::{post_json, redact_oauth_body_text, required_string};
use super::super::interaction::{AuthInteraction, AuthPrompt, SelectOption};
use super::super::pkce::generate_pkce;
use super::{now_ms, parse_authorization_input, LoginOptions, OAuthCredential, OAuthFlow};
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::ModelAuth;

/// Upstream decodes this from base64 at module load; the plaintext value is
/// `9d1c250a-e61b-44d9-88ed-5944d1962f5e`.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Preferred so the port can be forwarded into containers or over SSH;
/// Anthropic accepts any loopback port so login falls back to a free port.
pub const CALLBACK_PORT: u16 = 53692;
pub const CALLBACK_PATH: &str = "/callback";
pub const REDIRECT_URI: &str = "http://localhost:53692/callback";
const COPY_CODE_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
pub const BROWSER_LOGIN_METHOD: &str = "browser";
const COPY_CODE_LOGIN_METHOD: &str = "copy_code";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

/// Parse an Anthropic token endpoint response body into a stored credential
/// (public for the mock-server exchange tests; mirrors upstream's parse in
/// `exchangeAuthorizationCode`/`refreshAnthropicToken`).
pub fn credential_from_token(body: &serde_json::Value) -> AiResult<OAuthCredential> {
    let access = required_string(body, "access_token")?;
    let refresh = required_string(body, "refresh_token")?;
    let expires_in = super::super::http::positive_number(body, "expires_in")?;
    Ok(OAuthCredential::new(
        access,
        refresh,
        // Saturating: `expires_in` is provider-controlled, and the former
        // plain `+`/`-` panicked in debug builds (or wrapped in release) for
        // absurd values.
        now_ms()
            .saturating_add((expires_in * 1000.0) as i64)
            .saturating_sub(EXPIRY_MARGIN_MS),
    ))
}

async fn exchange_authorization_code(
    code: &str,
    state: &str,
    verifier: &str,
    redirect_uri: &str,
) -> AiResult<OAuthCredential> {
    let response = post_json(
        TOKEN_URL,
        &json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
        }),
    )
    .await
    .map_err(|error| {
        AiError::Authentication(format!(
            "Token exchange request failed. url={TOKEN_URL}; redirect_uri={redirect_uri}; \
             response_type=authorization_code; details={error}"
        ))
    })?;
    if !response.ok() {
        return Err(AiError::Authentication(format!(
            "HTTP request failed. status={}; url={TOKEN_URL}; body={}",
            response.status,
            redact_oauth_body_text(&response.body)
        )));
    }
    let body = response.json().map_err(|error| {
        AiError::Authentication(format!(
            "Token exchange returned invalid JSON. url={TOKEN_URL}; body={}; details={error}",
            redact_oauth_body_text(&response.body)
        ))
    })?;
    credential_from_token(&body)
}

async fn refresh_anthropic_token(refresh_token: &str) -> AiResult<OAuthCredential> {
    let response = post_json(
        TOKEN_URL,
        &json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh_token,
        }),
    )
    .await
    .map_err(|error| {
        AiError::Authentication(format!(
            "Anthropic token refresh request failed. url={TOKEN_URL}; details={error}"
        ))
    })?;
    if !response.ok() {
        return Err(AiError::Authentication(format!(
            "HTTP request failed. status={}; url={TOKEN_URL}; body={}",
            response.status,
            redact_oauth_body_text(&response.body)
        )));
    }
    let body = response.json().map_err(|error| {
        AiError::Authentication(format!(
            "Anthropic token refresh returned invalid JSON. url={TOKEN_URL}; body={}; details={error}",
            redact_oauth_body_text(&response.body)
        ))
    })?;
    credential_from_token(&body)
}

async fn login_browser(interaction: &dyn AuthInteraction) -> AiResult<OAuthCredential> {
    let host = super::callback_host();
    let pkce = generate_pkce();
    // Without a callback server, login continues with the pasted redirect URL.
    let callback = match OAuthCallbackServer::start(
        "Anthropic",
        &host,
        CALLBACK_PORT,
        CALLBACK_PATH,
        Some("localhost"),
        Some(&pkce.verifier),
    )
    .await
    {
        Ok(callback) => Some(callback),
        Err(_) => OAuthCallbackServer::start(
            "Anthropic",
            &host,
            0,
            CALLBACK_PATH,
            Some("localhost"),
            Some(&pkce.verifier),
        )
        .await
        .ok(),
    };
    let redirect_uri = callback
        .as_ref()
        .map(|callback| callback.redirect_uri().to_string())
        .unwrap_or_else(|| REDIRECT_URI.to_string());

    let result: AiResult<OAuthCredential> = async {
        let authorize_url = format!(
            "{AUTHORIZE_URL}?code=true&client_id={CLIENT_ID}&response_type=code&redirect_uri={}\
             &scope={}&code_challenge={}&code_challenge_method=S256&state={}",
            urlencode(&redirect_uri),
            urlencode(SCOPES),
            pkce.challenge,
            pkce.verifier,
        );
        interaction.notify(super::super::interaction::AuthEvent::AuthUrl {
            url: authorize_url,
            instructions: Some(
                "Complete login in your browser. If the browser is on another machine, paste the \
                 final redirect URL here."
                    .to_string(),
            ),
        });

        let (code, state) = match &callback {
            Some(callback) => {
                wait_callback_or_manual(
                    callback,
                    interaction,
                    AuthPrompt::ManualCode {
                        message:
                            "Complete login in your browser, or paste the authorization code / redirect URL here:"
                                .to_string(),
                        placeholder: Some(redirect_uri.clone()),
                    },
                    &pkce.verifier,
                )
                .await?
            }
            None => {
                let input = interaction
                    .prompt(AuthPrompt::ManualCode {
                        message:
                            "Complete login in your browser, or paste the authorization code / redirect URL here:"
                                .to_string(),
                        placeholder: Some(redirect_uri.clone()),
                    })
                    .await?;
                parse_code_input(&input, &pkce.verifier)?
            }
        };
        let Some(code) = code else {
            return Err(AiError::Authentication("Missing authorization code".to_string()));
        };
        let state = state.unwrap_or_else(|| pkce.verifier.clone());
        interaction.notify(super::super::interaction::AuthEvent::Progress {
            message: "Exchanging authorization code for tokens...".to_string(),
        });
        exchange_authorization_code(&code, &state, &pkce.verifier, &redirect_uri).await
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

/// Wait for the callback, racing the manual prompt; the losing side is
/// cancelled (upstream `waitForCallbackOrManualInput`). `expected_state`
/// validates manually pasted input; callback delivery is validated by the
/// server itself. The prompt closure form lets either branch construct the
/// prompt (only one of the two bodies runs).
pub(super) async fn wait_callback_or_manual(
    callback: &OAuthCallbackServer,
    interaction: &dyn AuthInteraction,
    prompt: AuthPrompt,
    expected_state: &str,
) -> AiResult<(Option<String>, Option<String>)> {
    let make_prompt = || interaction.prompt(prompt.clone());
    tokio::select! {
        wait = callback.wait(None) => {
            match wait? {
                CallbackWait::Completed(pairs) => {
                    // A provider-reported `?error=` callback settles the wait
                    // with the failure (upstream `finish({error})` rejects).
                    if let Some(error) =
                        super::super::callback_server::authorization_error(&pairs)
                    {
                        return Err(AiError::Authentication(format!(
                            "Anthropic authorization failed: {error}"
                        )));
                    }
                    let code = super::super::callback_server::query_param(&pairs, "code")
                        .ok_or_else(|| {
                            AiError::Authentication("Missing authorization code".to_string())
                        })?;
                    Ok((Some(code.to_string()), None))
                }
                CallbackWait::Cancelled => {
                    // The server side gave up (timeout/cancel); the manual
                    // prompt decides the outcome.
                    let input = make_prompt().await?;
                    Ok(parse_code_input(&input, expected_state)?)
                }
            }
        }
        manual = make_prompt() => {
            callback.cancel();
            let input = manual?;
            Ok(parse_code_input(&input, expected_state)?)
        }
    }
}

/// Parse manual input into `(code, state)`, validating the state when present.
pub(super) fn parse_code_input(
    input: &str,
    expected_state: &str,
) -> AiResult<(Option<String>, Option<String>)> {
    let (code, state) = parse_authorization_input(input);
    if let Some(state) = &state {
        if state != expected_state {
            return Err(AiError::Authentication("OAuth state mismatch".to_string()));
        }
    }
    Ok((code, state))
}

async fn login_copy_code(interaction: &dyn AuthInteraction) -> AiResult<OAuthCredential> {
    let pkce = generate_pkce();
    let authorize_url = format!(
        "{AUTHORIZE_URL}?code=true&client_id={CLIENT_ID}&response_type=code&redirect_uri={}\
         &scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        urlencode(COPY_CODE_REDIRECT_URI),
        urlencode(SCOPES),
        pkce.challenge,
        pkce.verifier,
    );
    interaction.notify(super::super::interaction::AuthEvent::AuthUrl {
        url: authorize_url,
        instructions: Some(
            "Complete login in your browser, then copy the code Anthropic shows and paste it here."
                .to_string(),
        ),
    });
    let input = interaction
        .prompt(AuthPrompt::ManualCode {
            message: "Paste the code Anthropic shows after you sign in:".to_string(),
            placeholder: Some("code#state".to_string()),
        })
        .await?;
    let (code, state) = parse_code_input(&input, &pkce.verifier)?;
    let Some(code) = code else {
        return Err(AiError::Authentication(
            "Missing authorization code".to_string(),
        ));
    };
    let state = state.unwrap_or_else(|| pkce.verifier.clone());
    interaction.notify(super::super::interaction::AuthEvent::Progress {
        message: "Exchanging authorization code for tokens...".to_string(),
    });
    exchange_authorization_code(&code, &state, &pkce.verifier, COPY_CODE_REDIRECT_URI).await
}

/// Minimal percent-encoding for query values (space, `&`, `=`, `?`, `#`).
pub(super) fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// The Anthropic OAuth flow.
pub struct AnthropicOAuth;

impl OAuthFlow for AnthropicOAuth {
    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
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
            let method = interaction
                .prompt(AuthPrompt::Select {
                    message: "Select Anthropic login method:".to_string(),
                    options: vec![
                        SelectOption {
                            id: BROWSER_LOGIN_METHOD.to_string(),
                            label: "Browser login (default)".to_string(),
                            description: None,
                        },
                        SelectOption {
                            id: COPY_CODE_LOGIN_METHOD.to_string(),
                            label: "Copy code login (headless)".to_string(),
                            description: None,
                        },
                    ],
                })
                .await?;
            match method.as_str() {
                COPY_CODE_LOGIN_METHOD => login_copy_code(interaction).await,
                BROWSER_LOGIN_METHOD => login_browser(interaction).await,
                other => Err(AiError::Authentication(format!(
                    "Unknown Anthropic login method: {other}"
                ))),
            }
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move { refresh_anthropic_token(&credential.refresh).await })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: None,
        })
    }
}
