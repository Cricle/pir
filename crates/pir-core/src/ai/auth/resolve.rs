//! Auth resolution and the locked OAuth refresh pattern.
//!
//! Rust equivalent of upstream `packages/ai/src/auth/resolve.ts`. A stored
//! credential owns the provider: ambient env is consulted only when nothing is
//! stored, and there is no silent env fallback after a failed refresh. The
//! refresh runs inside the credential-store lock in a detached task and its
//! persistence ignores caller cancellation (bounded only by a timeout), so a
//! cancelled caller can never discard the only valid refresh token.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use super::interaction::{AuthInteraction, LoginOptions};
use super::oauth::{OAuthCredential, OAuthFlow, OAuthFlowBox};
use super::storage::{FileAuthStorage, StoredCredential};
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::{ApiKeyCredential, AuthResult, ModelAuth};
use crate::plugins::services::{
    native_provider_auth_hooks, AuthPromptContext, ProviderAuth, ProviderAuthError,
    ProviderAuthHooks,
};

/// Require this much remaining OAuth-token validity (upstream
/// `DEFAULT_OAUTH_MINIMUM_VALIDITY_MS`).
const DEFAULT_OAUTH_MINIMUM_VALIDITY_MS: i64 = 5 * 60 * 1000;
/// The bound on the refresh network call itself, which ignores cancellation.
const DEFAULT_OAUTH_REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

/// Whether a credential needs a refresh (upstream's `expiresSoon`).
pub fn oauth_expires_soon(credential: &OAuthCredential, minimum_validity_ms: i64) -> bool {
    // Saturating: `expires` and the caller-supplied margin are both i64 and
    // the addition must not overflow on an absurd stored expiry.
    (crate::ai::util::now_ms() as i64)
        .saturating_add(minimum_validity_ms.max(DEFAULT_OAUTH_MINIMUM_VALIDITY_MS))
        >= credential.expires
}

/// Refresh a stored OAuth credential under the credential-store lock and
/// persist the result before the lock is released (upstream
/// `refreshStoredOAuthCredential`).
///
/// The refresh runs in a detached task: caller cancellation cancels only the
/// wait for the result, never the refresh+persist itself — once a refresh
/// starts, the provider may already have rotated the refresh token, so the
/// rotation is always persisted. `needs_refresh` is re-checked under the lock,
/// so concurrent callers refresh only once. Resolves with the stored OAuth
/// credential after the operation, or `None` when the provider no longer has
/// an OAuth credential (logged out meanwhile).
pub async fn refresh_stored_oauth_credential(
    storage: Arc<FileAuthStorage>,
    provider_id: &str,
    oauth: OAuthFlowBox,
    needs_refresh: impl Fn(&OAuthCredential) -> bool + Send + 'static,
) -> AiResult<Option<OAuthCredential>> {
    let provider = provider_id.to_string();
    // Native hook precedence: a registered hook is consulted first and the
    // builtin/JS flow runs exactly once, only when the hook reports
    // `Unavailable`.
    let hooks = native_provider_auth_hooks(provider_id);
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = storage
            .modify(provider.as_str(), |current| {
                let provider = provider.clone();
                let hooks = hooks.clone();
                async move {
                    let Some(StoredCredential::OAuth(credential)) = current else {
                        // Logged out meanwhile.
                        return Ok(None);
                    };
                    if !needs_refresh(&credential) {
                        // Another caller refreshed already.
                        return Ok(None);
                    }
                    if let Some(hooks) = hooks {
                        let current_auth = ProviderAuth::from_oauth(&credential);
                        let refreshed = tokio::time::timeout(
                            DEFAULT_OAUTH_REFRESH_TIMEOUT,
                            hooks.refresh(&current_auth),
                        )
                        .await;
                        match refreshed {
                            Ok(Ok(auth)) => {
                                let next = auth.as_oauth().ok_or_else(|| {
                                    AiError::Authentication(format!(
                                        "native provider auth refresh for {provider} returned \
                                         no OAuth tokens"
                                    ))
                                })?;
                                return Ok(Some(StoredCredential::OAuth(next)));
                            }
                            // The hook cannot refresh here: run the one
                            // builtin/JS fallback below.
                            Ok(Err(ProviderAuthError::Unavailable)) => {}
                            Ok(Err(error)) => {
                                return Err(provider_auth_error(&provider, error));
                            }
                            Err(_) => {
                                return Err(AiError::Authentication(format!(
                                    "native provider auth refresh for {provider} timed out \
                                     after {}s",
                                    DEFAULT_OAUTH_REFRESH_TIMEOUT.as_secs()
                                )));
                            }
                        }
                    }
                    let refreshed = tokio::time::timeout(
                        DEFAULT_OAUTH_REFRESH_TIMEOUT,
                        oauth.refresh(&credential),
                    )
                    .await;
                    match refreshed {
                        Ok(Ok(next)) => Ok(Some(StoredCredential::OAuth(next))),
                        Ok(Err(error)) => Err(AiError::Authentication(format!(
                            "OAuth refresh failed for {provider}: {error}"
                        ))),
                        Err(_) => Err(AiError::Authentication(format!(
                            "OAuth refresh failed for {provider}: timed out after {}s",
                            DEFAULT_OAUTH_REFRESH_TIMEOUT.as_secs()
                        ))),
                    }
                }
            })
            .await
            .map(|post| match post {
                Some(StoredCredential::OAuth(credential)) => Some(credential),
                _ => None,
            });
        // The caller may have cancelled; the persistence already happened.
        let _ = result_tx.send(result);
    });
    result_rx
        .await
        .map_err(|_| AiError::Authentication("OAuth refresh task failed".to_string()))?
}

/// Resolve request auth for a provider from the file-backed credential store,
/// mirroring upstream's precedence: a stored OAuth credential with a matching
/// OAuth handler wins (refreshing through the locked path when it expires
/// soon); a stored api-key credential resolves through the api-key path; only
/// a missing credential falls back to ambient sources.
pub async fn resolve_stored_auth(
    storage: &Arc<FileAuthStorage>,
    provider_id: &str,
    oauth_flow: Option<&OAuthFlowBox>,
    resolve_api_key: &(dyn Fn(Option<&crate::ai::types::ApiKeyCredential>) -> AiResult<Option<AuthResult>>
          + Send
          + Sync),
) -> AiResult<Option<AuthResult>> {
    let stored = storage.read_stored(provider_id)?;
    match stored {
        Some(StoredCredential::OAuth(credential)) => {
            let Some(flow) = oauth_flow else {
                // Stored credential without a matching handler: not configured,
                // no silent env fallback (upstream resolve.ts).
                return Ok(None);
            };
            let credential =
                resolve_oauth_credential(storage.clone(), provider_id, flow.clone(), credential)
                    .await?;
            let auth = flow.to_auth(&credential).map_err(|error| {
                AiError::Authentication(format!(
                    "OAuth auth derivation failed for {provider_id}: {error}"
                ))
            })?;
            Ok(Some(AuthResult {
                auth,
                env: None,
                source: Some("OAuth".to_string()),
            }))
        }
        Some(StoredCredential::ApiKey(credential)) => resolve_api_key(Some(&credential)),
        None => resolve_api_key(None),
    }
}

/// Shared OAuth refresh-or-reuse step used by resolution: refresh through
/// [`refresh_stored_oauth_credential`] when the stored token expires soon,
/// then return the post-operation stored credential.
pub async fn resolve_oauth_credential(
    storage: Arc<FileAuthStorage>,
    provider_id: &str,
    flow: OAuthFlowBox,
    stored: OAuthCredential,
) -> AiResult<OAuthCredential> {
    if !oauth_expires_soon(&stored, DEFAULT_OAUTH_MINIMUM_VALIDITY_MS) {
        return Ok(stored);
    }
    refresh_stored_oauth_credential(storage, provider_id, flow, |credential| {
        oauth_expires_soon(credential, DEFAULT_OAUTH_MINIMUM_VALIDITY_MS)
    })
    .await?
    .ok_or_else(|| AiError::Authentication(format!("provider {provider_id} logged out")))
}

/// Resolve one provider's bare request token through the same credential
/// chain request auth uses (upstream `Models.getAuth(providerId).auth.apiKey`),
/// for a consumer that owns no provider registry: MCP `auth: { provider }`.
///
/// Precedence:
///
/// 1. a stored OAuth credential resolves through the provider's OAuth flow,
///    refreshing inside the credential-store lock when it expires soon;
/// 2. a stored api-key credential resolves as that key (a stored credential
///    with no usable key stays unconfigured, never borrowing another source);
/// 3. only a missing credential falls back to the provider's `models.json`
///    `apiKey` (templates resolved against `agent_dir`) and then to the
///    provider's environment keys.
///
/// `agent_dir` is the caller's ONE root: `auth.json` and `models.json` are
/// read from it, and a `!command` models.json key resolves with its cache
/// scoped to it. An empty root (a manager built without one) resolves only
/// the environment keys.
pub async fn resolve_provider_token(
    agent_dir: &Path,
    provider_id: &str,
) -> AiResult<Option<String>> {
    if agent_dir.as_os_str().is_empty() {
        return Ok(provider_env_token(provider_id));
    }
    let storage = Arc::new(FileAuthStorage::in_agent_dir(agent_dir));
    let flow = oauth_flow_for(provider_id, None);
    resolve_provider_token_with(&storage, agent_dir, provider_id, flow.as_ref()).await
}

/// [`resolve_provider_token`] after the server rejected the token with a 401:
/// a stored OAuth credential that still carries the rejected access token is
/// refreshed under the credential-store lock even when its `expires` has not
/// passed yet (a server can revoke a token early), and the post-refresh token
/// is returned.
///
/// A credential another caller already rotated is used as-is (one shared
/// refresh); a refresh failure surfaces as [`AiError::Authentication`] so the
/// caller reports a sign-in requirement instead of retrying the stale value.
/// `rejected` is the token the failed request carried, so a stored credential
/// that moved on meanwhile is not refreshed a second time.
pub async fn refresh_provider_token_after_rejection(
    agent_dir: &Path,
    provider_id: &str,
    rejected: Option<&str>,
) -> AiResult<Option<String>> {
    if agent_dir.as_os_str().is_empty() {
        return Ok(provider_env_token(provider_id));
    }
    let storage = Arc::new(FileAuthStorage::in_agent_dir(agent_dir));
    let flow = oauth_flow_for(provider_id, None);
    let stored = storage.read_stored(provider_id)?;
    let Some(flow) = flow else {
        return resolve_provider_token_with(&storage, agent_dir, provider_id, None).await;
    };
    let Some(StoredCredential::OAuth(stored)) = stored else {
        // No OAuth credential to refresh: an api-key credential (or nothing)
        // re-resolves through the normal chain; the caller compares the result
        // with the rejected token and retries only when it changed.
        return resolve_provider_token_with(&storage, agent_dir, provider_id, Some(&flow)).await;
    };
    if rejected.is_some_and(|rejected| stored.access != rejected) {
        // The store moved on since the failed request was sent (a refresh or
        // `/login` in another task/process): use the current credential.
        return oauth_provider_token(flow.as_ref(), &stored, provider_id);
    }
    let rejected = rejected.map(str::to_owned);
    let post = refresh_stored_oauth_credential(
        storage.clone(),
        provider_id,
        flow.clone(),
        // `None` (the request carried no token) still tries a refresh: the
        // OAuth grant may be able to produce one.
        move |current| {
            rejected
                .as_deref()
                .is_none_or(|rejected| current.access == rejected)
        },
    )
    .await?;
    let credential = match post {
        Some(credential) => credential,
        // Another caller refreshed between the read and the lock: use the
        // credential that is stored now. `None` here means logged out
        // meanwhile, and a stored credential owns the provider (no fallback).
        None => match storage.read_stored(provider_id)? {
            Some(StoredCredential::OAuth(current)) => current,
            _ => return Ok(None),
        },
    };
    oauth_provider_token(flow.as_ref(), &credential, provider_id)
}

/// Shared body of the two token resolvers: [`resolve_stored_auth`] with the
/// models.json/environment fallback only for the missing-credential case.
async fn resolve_provider_token_with(
    storage: &Arc<FileAuthStorage>,
    agent_dir: &Path,
    provider_id: &str,
    flow: Option<&OAuthFlowBox>,
) -> AiResult<Option<String>> {
    let resolved = resolve_stored_auth(storage, provider_id, flow, &|credential| {
        Ok(configured_provider_auth(agent_dir, provider_id, credential))
    })
    .await?;
    Ok(resolved
        .and_then(|resolved| resolved.auth.api_key)
        .filter(|token| !token.is_empty()))
}

/// The api-key half of the shared chain: a stored credential wins, otherwise
/// the provider's models.json key and then its environment keys.
fn configured_provider_auth(
    agent_dir: &Path,
    provider_id: &str,
    credential: Option<&ApiKeyCredential>,
) -> Option<AuthResult> {
    let (key, source) = match credential {
        Some(credential) => (
            credential
                .key
                .clone()
                .filter(|key| !key.trim().is_empty())?,
            "stored credential",
        ),
        None => (
            models_json_provider_key(agent_dir, provider_id)
                .or_else(|| provider_env_token(provider_id))?,
            "configured API key",
        ),
    };
    Some(AuthResult {
        auth: ModelAuth {
            api_key: Some(key),
            headers: None,
            base_url: None,
        },
        env: credential.and_then(|credential| credential.env.clone()),
        source: Some(source.to_string()),
    })
}

/// The provider's `models.json` api key (`providers.<id>.apiKey`), resolved
/// through the shared config-value templates. A missing/malformed models.json
/// yields no key: this path has no config-error surface and must not block the
/// credential sources around it.
fn models_json_provider_key(agent_dir: &Path, provider_id: &str) -> Option<String> {
    let contents = std::fs::read_to_string(agent_dir.join("models.json")).ok()?;
    let config = crate::ai::providers::parse_pi_config(&contents).ok()?;
    let key = config.providers.get(provider_id)?.api_key.as_deref()?;
    crate::ai::providers::config_value::resolve_config_value_in(agent_dir, key, None)
        .filter(|key| !key.trim().is_empty())
}

/// The first non-empty environment api key of `provider` (the same env vars a
/// provider's api-key auth reads, in precedence order).
fn provider_env_token(provider: &str) -> Option<String> {
    crate::ai::auth::env_api_keys::all()
        .into_iter()
        .find(|(id, _)| *id == provider)
        .and_then(|(_, names)| {
            names
                .iter()
                .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        })
}

/// The bearer token a stored OAuth credential derives for `provider`, with
/// the raw access token as a fallback for flows whose `to_auth` derives only
/// headers.
fn oauth_provider_token(
    flow: &dyn OAuthFlow,
    credential: &OAuthCredential,
    provider_id: &str,
) -> AiResult<Option<String>> {
    let auth = flow.to_auth(credential).map_err(|error| {
        AiError::Authentication(format!(
            "OAuth auth derivation failed for {provider_id}: {error}"
        ))
    })?;
    Ok(auth
        .api_key
        .filter(|token| !token.is_empty())
        .or_else(|| Some(credential.access.clone()).filter(|token| !token.is_empty())))
}

/// Map a native provider-auth hook failure onto the auth error surface.
///
/// `Unavailable` is handled by the callers as a fallback signal and never
/// reaches this mapping on a successful builtin path.
fn provider_auth_error(provider_id: &str, error: ProviderAuthError) -> AiError {
    match error {
        ProviderAuthError::Failed(error) => AiError::Authentication(format!(
            "native provider auth failed for {provider_id}: {error}"
        )),
        ProviderAuthError::Unavailable => AiError::Authentication(format!(
            "native provider auth hook for {provider_id} is unavailable"
        )),
        ProviderAuthError::Cancelled => {
            AiError::Authentication(format!("login cancelled for {provider_id}"))
        }
    }
}

/// Convert a native hook result into the stored credential shape.
///
/// OAuth results become `StoredCredential::OAuth` (the same shape
/// `login_and_store` writes for builtin flows); api-key results become
/// `StoredCredential::ApiKey`.
pub fn provider_auth_to_stored(auth: &ProviderAuth) -> AiResult<StoredCredential> {
    if let Some(credential) = auth.as_oauth() {
        return Ok(StoredCredential::OAuth(credential));
    }
    if let Some(key) = auth.api_key.as_deref().filter(|key| !key.trim().is_empty()) {
        return Ok(StoredCredential::ApiKey(ApiKeyCredential::new(Some(
            key.to_owned(),
        ))));
    }
    Err(AiError::Authentication(
        "native provider auth hook returned neither an API key nor OAuth tokens".to_string(),
    ))
}

/// Login orchestration: run the native hook when one is registered, otherwise
/// the flow's login, persist the credential, and return it. Cancellation of
/// the login future aborts before any token is stored (upstream: the app
/// persists via `modify` after login).
///
/// Native hooks win over the builtin/JS `flow` for the same provider. The
/// fallback runs exactly once, and only when no hook is registered or the
/// hook reports [`ProviderAuthError::Unavailable`]; a `Failed`/`Cancelled`
/// hook returns a typed error before any store write, leaving the previous
/// credential intact.
pub async fn login_and_store(
    storage: &FileAuthStorage,
    provider_id: &str,
    flow: &dyn OAuthFlow,
    interaction: &dyn AuthInteraction,
    options: &LoginOptions,
) -> AiResult<ProviderAuth> {
    if let Some(hooks) = native_provider_auth_hooks(provider_id) {
        let context = AuthPromptContext {
            interaction,
            options,
        };
        match hooks.login(context).await {
            Ok(auth) => {
                let stored = provider_auth_to_stored(&auth)?;
                storage.store(provider_id, stored)?;
                return Ok(auth);
            }
            Err(ProviderAuthError::Unavailable) => {
                // Explicit fallback signal: run the builtin/JS flow once.
            }
            Err(error) => return Err(provider_auth_error(provider_id, error)),
        }
    }
    let credential = flow.login(interaction, options).await?;
    storage.store(provider_id, StoredCredential::OAuth(credential.clone()))?;
    Ok(ProviderAuth::from_oauth(&credential))
}

/// Status summary for one provider's stored credential (no secret material).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredAuthStatus {
    Missing,
    ApiKey,
    OAuth { expires: i64 },
}

pub fn stored_status(storage: &FileAuthStorage, provider_id: &str) -> AiResult<StoredAuthStatus> {
    Ok(match storage.read_stored(provider_id)? {
        None => StoredAuthStatus::Missing,
        Some(StoredCredential::ApiKey(_)) => StoredAuthStatus::ApiKey,
        Some(StoredCredential::OAuth(credential)) => StoredAuthStatus::OAuth {
            expires: credential.expires,
        },
    })
}

/// An [`OAuthFlow`] backed by a native [`ProviderAuthHooks`] registration.
///
/// Returned by [`oauth_flow_for`] so every existing flow consumer (the TUI
/// login selector, the model catalog's `Provider::oauth_flow`, the CLI
/// `pir auth login` path) discovers the native hook through the standard flow
/// interface. `login_and_store` / `refresh_stored_oauth_credential` consult
/// the hook directly first, so this adapter's methods are normally reached
/// only by callers that invoke a flow outside those orchestrators. The one
/// exception is a hook that answers `Unavailable`: the orchestrator then runs
/// this flow as the fallback, which reports `Unavailable` again for a
/// hook-only provider and never writes a credential.
struct NativeProviderAuthFlow {
    provider_id: String,
    hooks: Arc<dyn ProviderAuthHooks>,
}

impl NativeProviderAuthFlow {
    fn new(provider_id: &str, hooks: Arc<dyn ProviderAuthHooks>) -> Self {
        Self {
            provider_id: provider_id.to_string(),
            hooks,
        }
    }
}

impl OAuthFlow for NativeProviderAuthFlow {
    fn name(&self) -> &str {
        "native provider auth"
    }

    fn is_subscription(&self) -> bool {
        self.hooks.is_subscription()
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
        options: &'a LoginOptions,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        let hooks = Arc::clone(&self.hooks);
        let provider = self.provider_id.clone();
        Box::pin(async move {
            let context = AuthPromptContext {
                interaction,
                options,
            };
            let auth = hooks
                .login(context)
                .await
                .map_err(|error| provider_auth_error(&provider, error))?;
            auth.as_oauth().ok_or_else(|| {
                AiError::Authentication(format!(
                    "native provider auth hook for {provider} returned no OAuth credentials"
                ))
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        let hooks = Arc::clone(&self.hooks);
        let provider = self.provider_id.clone();
        let current = ProviderAuth::from_oauth(credential);
        Box::pin(async move {
            let auth = hooks
                .refresh(&current)
                .await
                .map_err(|error| provider_auth_error(&provider, error))?;
            auth.as_oauth().ok_or_else(|| {
                AiError::Authentication(format!(
                    "native provider auth refresh for {provider} returned no OAuth credentials"
                ))
            })
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        let auth = ProviderAuth::from_oauth(credential);
        Ok(ModelAuth {
            api_key: auth.api_key.or(auth.access_token),
            headers: None,
            base_url: None,
        })
    }
}

/// The flows pir implements, keyed by provider id (upstream's set at HEAD:
/// anthropic, openai (ChatGPT), openai-codex, github-copilot, openrouter,
/// kimi-coding, xai, radius, meta).
///
/// A registered native [`ProviderAuthHooks`] wins over the builtin flow for
/// the same provider id and is returned as a [`NativeProviderAuthFlow`].
pub fn oauth_flow_for(provider_id: &str, gateway: Option<&str>) -> Option<OAuthFlowBox> {
    if let Some(hooks) = native_provider_auth_hooks(provider_id) {
        return Some(Arc::new(NativeProviderAuthFlow::new(provider_id, hooks)));
    }
    let flow: OAuthFlowBox = match provider_id {
        "anthropic" => Arc::new(super::oauth::anthropic::AnthropicOAuth),
        "openai" => Arc::new(super::oauth::openai_chatgpt::OpenAiChatGptOAuth),
        "openai-codex" => Arc::new(super::oauth::openai_codex::OpenAiCodexOAuth),
        "github-copilot" => Arc::new(super::oauth::github_copilot::GitHubCopilotOAuth),
        "openrouter" => Arc::new(super::oauth::openrouter::OpenRouterOAuth),
        "kimi-coding" => Arc::new(super::oauth::kimi_coding::KimiCodingOAuth),
        "xai" => Arc::new(super::oauth::xai::XaiOAuth),
        "radius" => {
            let gateway = super::oauth::radius::resolve_radius_gateway(gateway);
            Arc::new(super::oauth::radius::RadiusOAuth::new("Radius", &gateway))
        }
        "meta" => Arc::new(super::oauth::meta::MetaOAuth),
        _ => return None,
    };
    Some(flow)
}

/// The provider ids with built-in OAuth flows.
pub const OAUTH_PROVIDERS: &[&str] = &[
    "anthropic",
    "github-copilot",
    "kimi-coding",
    "meta",
    "openai",
    "openai-codex",
    "openrouter",
    "radius",
    "xai",
];
