//! OpenRouter OAuth PKCE flow.
//!
//! Rust equivalent of upstream `packages/ai/src/auth/oauth/openrouter.ts`:
//! OpenRouter exchanges an authorization code for a permanent user-controlled
//! API key. The callback runs on an ephemeral loopback port with a random
//! path (no `state`); `refresh` is a no-op identity.

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use super::super::callback_server::{CallbackWait, OAuthCallbackServer};
use super::super::interaction::{AuthEvent, AuthInteraction, AuthPrompt};
use super::super::pkce::generate_pkce;
use super::{LoginOptions, OAuthCredential, OAuthFlow};
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::ModelAuth;

pub const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
pub const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const TOKEN_EXCHANGE_TIMEOUT_MS: u64 = 30_000;

/// `Number.MAX_SAFE_INTEGER` — a permanent key never expires.
const NEVER_EXPIRES: i64 = 9_007_199_254_740_991;

async fn exchange_authorization_code(code: &str, verifier: &str) -> AiResult<OAuthCredential> {
    let response = tokio::time::timeout(
        std::time::Duration::from_millis(TOKEN_EXCHANGE_TIMEOUT_MS),
        super::super::http::post_json(
            TOKEN_URL,
            &json!({
                "code": code,
                "code_verifier": verifier,
                "code_challenge_method": "S256",
            }),
        ),
    )
    .await
    .map_err(|_| {
        AiError::Authentication("OpenRouter OAuth token exchange timed out".to_string())
    })??;
    let status = response.status;
    let body: Value = if response.body.trim().is_empty() {
        Value::Null
    } else {
        response
            .json()
            .map_err(|error| {
                if (200..300).contains(&status) {
                    AiError::Authentication(format!(
                        "OpenRouter OAuth returned invalid JSON: {error}"
                    ))
                } else {
                    error
                }
            })
            .unwrap_or(Value::Null)
    };
    if !(200..300).contains(&status) {
        let detail = detail_from(&body)
            .unwrap_or_else(|| crate::ai::auth::http::redact_oauth_body_text(&body.to_string()));
        return Err(AiError::Authentication(format!(
            "OpenRouter OAuth key exchange failed (HTTP {status}): {detail}"
        )));
    }
    let key = body.get("key").and_then(Value::as_str).unwrap_or("");
    if key.is_empty() {
        return Err(AiError::Authentication(
            "OpenRouter OAuth response carries no \"key\"".to_string(),
        ));
    }
    Ok(OAuthCredential::new(key, "", NEVER_EXPIRES))
}

fn detail_from(body: &Value) -> Option<String> {
    for key in ["error_description", "message", "error"] {
        if let Some(value) = body.get(key).and_then(Value::as_str) {
            return Some(value.to_string());
        }
    }
    if let Some(message) = body
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        return Some(message.to_string());
    }
    None
}

async fn login(interaction: &dyn AuthInteraction) -> AiResult<OAuthCredential> {
    let pkce = generate_pkce();
    // OpenRouter sends no `state`; the random path keeps stray requests from
    // completing the sign-in.
    let callback = OAuthCallbackServer::start(
        "OpenRouter",
        &super::callback_host(),
        0,
        &format!("/oauth/callback/{}", uuid::Uuid::new_v4()),
        None,
        None,
    )
    .await?;
    let redirect_uri = callback.redirect_uri().to_string();

    let result: AiResult<OAuthCredential> = async {
        interaction.notify(AuthEvent::Progress {
            message: format!("Listening for OpenRouter OAuth callback on {redirect_uri}"),
        });
        let authorize_url = format!(
            "{AUTHORIZE_URL}?callback_url={}&code_challenge={}&code_challenge_method=S256",
            super::anthropic::urlencode(&redirect_uri),
            pkce.challenge,
        );
        interaction.notify(AuthEvent::AuthUrl {
            url: authorize_url,
            instructions: Some(
                "Complete sign-in in your browser. If the browser is on another machine, paste the \
                 final redirect URL here."
                    .to_string(),
            ),
        });

        let prompt = || {
            interaction.prompt(AuthPrompt::ManualCode {
                message:
                    "Complete sign-in in your browser, or paste the authorization code / redirect URL here:"
                        .to_string(),
                placeholder: Some(redirect_uri.clone()),
            })
        };
        let code = tokio::select! {
            wait = callback.wait(None) => match wait? {
                CallbackWait::Completed(pairs) => {
                    // A provider-reported `?error=` callback settles the wait
                    // with the failure (upstream `finish({error})` rejects).
                    if let Some(error) =
                        super::super::callback_server::authorization_error(&pairs)
                    {
                        return Err(crate::ai::error::AiError::Authentication(format!(
                            "OpenRouter authorization failed: {error}"
                        )));
                    }
                    super::super::callback_server::query_param(&pairs, "code")
                        .ok_or_else(|| {
                            crate::ai::error::AiError::Authentication(
                                "Missing authorization code".to_string(),
                            )
                        })?
                        .to_string()
                }
                CallbackWait::Cancelled => manual_code(&prompt().await?)?,
            },
            manual = prompt() => {
                callback.cancel();
                manual_code(&manual?)?
            }
        };
        interaction.notify(AuthEvent::Progress {
            message: "Exchanging authorization code for an API key...".to_string(),
        });
        exchange_authorization_code(&code, &pkce.verifier).await
    }
    .await;
    // Answer the browser page with the real exchange outcome (upstream sends
    // the page only after `complete` settles): success renders 200, a failure
    // renders 502 carrying the error.
    callback.confirm_page(result.as_ref().err().map(|error| error.to_string()));
    callback.close();
    result
}

fn manual_code(input: &str) -> AiResult<String> {
    let value = input.trim();
    if value.is_empty() {
        return Err(AiError::Authentication(
            "Missing authorization code".to_string(),
        ));
    }
    if let Some((_, query)) = value.split_once('?') {
        let pairs = super::super::callback_server::parse_query(query);
        if let Some(code) = super::super::callback_server::query_param(&pairs, "code") {
            return Ok(code.to_string());
        }
    }
    Ok(value.to_string())
}

/// The OpenRouter OAuth flow.
pub struct OpenRouterOAuth;

impl OAuthFlow for OpenRouterOAuth {
    fn name(&self) -> &str {
        "OpenRouter OAuth"
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with OpenRouter")
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
        // Permanent keys never rotate.
        Box::pin(std::future::ready(Ok(credential.clone())))
    }

    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: None,
        })
    }
}
