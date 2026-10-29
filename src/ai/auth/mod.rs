//! Authentication for AI providers: API keys, OAuth login flows, credential
//! storage, and environment reading.
//!
//! Rust equivalent of the upstream `packages/ai/src/auth/*` (contexts,
//! credential stores, resolve) plus `packages/ai/src/auth/oauth/*` (the
//! provider login flows) and `packages/coding-agent/src/core/auth-storage.ts`
//! (the `~/.pi/agent/auth.json` file store).

pub mod callback_server;
pub mod cli;
pub mod device_code;
pub mod http;
pub mod interaction;
pub mod oauth;
pub mod oauth_page;
pub mod pkce;
pub mod resolve;
pub mod storage;

pub use interaction::{
    AuthEvent, AuthInfoLink, AuthInteraction, AuthPrompt, LoginOptions, SelectOption,
};
pub use oauth::{OAuthCredential, OAuthFlow, OAuthFlowBox};
pub use resolve::{
    login_and_store, oauth_expires_soon, oauth_flow_for, refresh_stored_oauth_credential,
    resolve_stored_auth, stored_status, StoredAuthStatus, OAUTH_PROVIDERS,
};
pub use storage::{
    default_auth_path, AuthStorageData, FileAuthStorage, StoredCredential, StoredCredentialType,
};

use std::collections::BTreeMap;

use crate::ai::error::AiResult;
use crate::ai::types::{
    ApiKeyCredential, AuthCheck, AuthResult, AuthType, Credential, CredentialType, ModelAuth,
    ProviderEnv, ProviderHeaders,
};

// ---------------------------------------------------------------------------
// Auth context
// ---------------------------------------------------------------------------

/// Environment access for auth resolution. Injectable for tests.
pub trait AuthContext: Send + Sync {
    /// Read a single environment variable; `None` when unset or empty.
    fn env(&self, name: &str) -> AiResult<Option<String>>;

    /// Check whether a file exists. Always false for non-file contexts.
    fn file_exists(&self, path: &str) -> AiResult<bool>;
}

/// Default auth context backed by the process environment.
///
/// Values that are empty or whitespace-only count as unset, matching the
/// upstream default auth context.
pub struct EnvAuthContext {
    _private: (),
}

impl EnvAuthContext {
    /// Create a new env auth context.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for EnvAuthContext {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthContext for EnvAuthContext {
    fn env(&self, name: &str) -> AiResult<Option<String>> {
        let value = std::env::var(name).ok();
        match value {
            Some(value) if !value.trim().is_empty() => Ok(Some(value)),
            _ => Ok(None),
        }
    }

    fn file_exists(&self, path: &str) -> AiResult<bool> {
        let resolved = if let Some(rest) = path.strip_prefix("~") {
            std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .map(|home| format!("{home}{rest}"))
                .unwrap_or_else(|_| path.to_string())
        } else {
            path.to_string()
        };
        Ok(std::path::Path::new(&resolved).is_file())
    }
}

/// An in-memory auth context for tests.
pub struct MapAuthContext {
    values: BTreeMap<String, String>,
}

impl MapAuthContext {
    /// Create a context from the given env pairs.
    pub fn new(values: BTreeMap<String, String>) -> Self {
        Self { values }
    }
}

impl AuthContext for MapAuthContext {
    fn env(&self, name: &str) -> AiResult<Option<String>> {
        Ok(self
            .values
            .get(name)
            .cloned()
            .filter(|v| !v.trim().is_empty()))
    }

    fn file_exists(&self, _path: &str) -> AiResult<bool> {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Credential store
// ---------------------------------------------------------------------------

/// Non-secret credential metadata for account/status enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialInfo {
    pub provider_id: String,
    pub credential_type: CredentialType,
}

/// App-owned credential storage, keyed by provider id, one credential per
/// provider.
///
/// `modify` is the only write path, so every mutation is a serialized
/// read-modify-write. Rust equivalent of the upstream `CredentialStore`.
pub trait CredentialStore: Send + Sync {
    /// Read the stored credential, possibly expired. `Ok(None)` for missing
    /// entries.
    fn read(&self, provider_id: &str) -> AiResult<Option<Credential>>;

    /// List stored credential metadata without exposing secrets.
    fn list(&self) -> AiResult<Vec<CredentialInfo>>;

    /// Serialized write — the only write path. `fn` sees the current
    /// credential; return the new credential, or `None` to leave the entry
    /// unchanged. Mutual exclusion per provider id.
    fn modify(
        &self,
        provider_id: &str,
        f: &mut dyn FnMut(Option<&Credential>) -> AiResult<Option<Credential>>,
    ) -> AiResult<Option<Credential>>;

    /// Remove a credential (logout).
    fn delete(&self, provider_id: &str) -> AiResult<()>;
}

/// Default in-memory credential store.
///
/// Writes are serialized per provider through a per-key mutex chain.
pub struct InMemoryCredentialStore {
    inner: std::sync::Mutex<BTreeMap<String, Credential>>,
}

impl InMemoryCredentialStore {
    /// Create an empty in-memory store.
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(BTreeMap::new()),
        }
    }
}

impl Default for InMemoryCredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn read(&self, provider_id: &str) -> AiResult<Option<Credential>> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(guard.get(provider_id).cloned())
    }

    fn list(&self) -> AiResult<Vec<CredentialInfo>> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(guard
            .iter()
            .map(|(provider_id, credential)| CredentialInfo {
                provider_id: provider_id.clone(),
                credential_type: credential.credential_type(),
            })
            .collect())
    }

    fn modify(
        &self,
        provider_id: &str,
        f: &mut dyn FnMut(Option<&Credential>) -> AiResult<Option<Credential>>,
    ) -> AiResult<Option<Credential>> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = guard.get(provider_id);
        let next = f(current)?;
        if let Some(next) = &next {
            guard.insert(provider_id.to_string(), next.clone());
        }
        Ok(next.or_else(|| guard.get(provider_id).cloned()))
    }

    fn delete(&self, provider_id: &str) -> AiResult<()> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.remove(provider_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// API key auth
// ---------------------------------------------------------------------------

/// Api-key auth: stored key plus ambient sources (env vars).
pub struct ApiKeyAuth {
    /// Display name, e.g. "Anthropic API key".
    pub name: String,
    /// Environment variables checked in order when no credential is stored.
    pub env_vars: Vec<String>,
    /// Environment variables resolved as `Authorization: Bearer <value>`
    /// request headers rather than api keys (upstream ANTHROPIC_AUTH_TOKEN,
    /// `providers/anthropic.ts`); checked before [`Self::env_vars`].
    pub bearer_env_vars: Vec<String>,
    /// Workload-identity federation fallback (upstream
    /// `providers/anthropic.ts:49-70`): resolved LAST, after stored keys and
    /// every env var, so keys keep winning.
    pub federation: Option<FederationResolver>,
}

/// The workload-identity federation resolver signature (upstream resolves it
/// inline in the Anthropic provider's ApiKeyAuth).
pub type FederationResolver = fn(&dyn AuthContext) -> AiResult<Option<AuthResult>>;

/// Anthropic workload-identity federation (upstream
/// `providers/anthropic.ts:49-70`): when
/// `ANTHROPIC_FEDERATION_RULE_ID`, `ANTHROPIC_ORGANIZATION_ID`, and
/// `ANTHROPIC_IDENTITY_TOKEN_FILE` are all set (the optional
/// `ANTHROPIC_SERVICE_ACCOUNT_ID` / `ANTHROPIC_WORKSPACE_ID` ride along), the
/// provider resolves to header-less auth carrying the federation ids in `env`
/// (they are provider config, not credentials). The identity token itself is
/// exchanged for a short-lived access token by the Anthropic SDK upstream;
/// pir has no SDK, so the exchange is not performed here (documented
/// deviation) — the ids travel in the request env for the transport layer.
pub fn anthropic_workload_identity_federation(
    ctx: &dyn AuthContext,
) -> AiResult<Option<AuthResult>> {
    const FEDERATION_RULE_ID: &str = "ANTHROPIC_FEDERATION_RULE_ID";
    const ORGANIZATION_ID: &str = "ANTHROPIC_ORGANIZATION_ID";
    const IDENTITY_TOKEN_FILE: &str = "ANTHROPIC_IDENTITY_TOKEN_FILE";
    const SERVICE_ACCOUNT_ID: &str = "ANTHROPIC_SERVICE_ACCOUNT_ID";
    const WORKSPACE_ID: &str = "ANTHROPIC_WORKSPACE_ID";
    let mut federation: BTreeMap<String, String> = BTreeMap::new();
    for env_var in [FEDERATION_RULE_ID, ORGANIZATION_ID, IDENTITY_TOKEN_FILE] {
        let Some(value) = ctx.env(env_var)? else {
            return Ok(None);
        };
        federation.insert(env_var.to_string(), value);
    }
    for env_var in [SERVICE_ACCOUNT_ID, WORKSPACE_ID] {
        if let Some(value) = ctx.env(env_var)? {
            federation.insert(env_var.to_string(), value);
        }
    }
    Ok(Some(AuthResult {
        auth: ModelAuth {
            api_key: None,
            headers: None,
            base_url: None,
        },
        env: Some(federation),
        source: Some("workload identity federation".to_string()),
    }))
}

impl ApiKeyAuth {
    /// Create an api-key auth for the given environment variables.
    pub fn new(name: impl Into<String>, env_vars: Vec<String>) -> Self {
        Self {
            name: name.into(),
            env_vars,
            bearer_env_vars: Vec::new(),
            federation: None,
        }
    }

    /// Attach `Authorization: Bearer` env vars (upstream ANTHROPIC_AUTH_TOKEN
    /// semantics).
    pub fn with_bearer_env_vars(mut self, vars: Vec<String>) -> Self {
        self.bearer_env_vars = vars;
        self
    }

    /// Attach the workload-identity federation fallback (upstream Anthropic
    /// provider's resolve tail).
    pub fn with_federation(mut self, resolver: FederationResolver) -> Self {
        self.federation = Some(resolver);
        self
    }

    /// Resolve auth from the stored credential and/or ambient sources.
    ///
    /// Returns `Ok(None)` when neither the credential nor any env var is set.
    pub fn resolve(
        &self,
        ctx: &dyn AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> AiResult<Option<AuthResult>> {
        if let Some(key) = credential.and_then(|c| c.key.as_ref()) {
            if !key.trim().is_empty() {
                return Ok(Some(AuthResult {
                    auth: ModelAuth {
                        api_key: Some(key.clone()),
                        headers: None,
                        base_url: None,
                    },
                    env: credential.and_then(|c| c.env.clone()),
                    source: Some("stored credential".to_string()),
                }));
            }
        }
        for env_var in &self.bearer_env_vars {
            if let Some(value) = ctx.env(env_var)? {
                let mut headers = BTreeMap::new();
                headers.insert("Authorization".to_string(), format!("Bearer {value}"));
                return Ok(Some(AuthResult {
                    auth: ModelAuth {
                        api_key: None,
                        headers: Some(headers),
                        base_url: None,
                    },
                    env: None,
                    source: Some(env_var.clone()),
                }));
            }
        }
        for env_var in &self.env_vars {
            if let Some(value) = ctx.env(env_var)? {
                return Ok(Some(AuthResult {
                    auth: ModelAuth {
                        api_key: Some(value),
                        headers: None,
                        base_url: None,
                    },
                    env: None,
                    source: Some(env_var.clone()),
                }));
            }
        }
        // Workload identity federation: last in line so keys and bearer env
        // vars keep winning (upstream providers/anthropic.ts:49-70).
        if let Some(federation) = &self.federation {
            return federation(ctx);
        }
        Ok(None)
    }

    /// Side-effect-free availability check.
    pub fn check(
        &self,
        ctx: &dyn AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> AiResult<Option<AuthCheck>> {
        match self.resolve(ctx, credential)? {
            Some(result) => Ok(Some(AuthCheck {
                source: result.source,
                auth_type: AuthType::ApiKey,
            })),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Cloudflare auth (upstream `providers/cloudflare-auth.ts` +
// `providers/cloudflare-stream.ts`)
// ---------------------------------------------------------------------------

/// Cloudflare env/config names (upstream `cloudflare-auth.ts` constants).
pub mod cloudflare {
    pub const API_KEY: &str = "CLOUDFLARE_API_KEY";
    pub const ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
    pub const GATEWAY_ID: &str = "CLOUDFLARE_GATEWAY_ID";
}

/// Which Cloudflare surface a credential resolves for (upstream
/// `CloudflareAuthKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudflareAuthKind {
    WorkersAi,
    AiGateway,
}

/// A resolved Cloudflare credential (upstream `resolveCloudflareEnv` return
/// shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudflareResolvedAuth {
    pub api_key: String,
    /// Provider env carrying the account id (and gateway id for AI Gateway);
    /// feeds the `{CLOUDFLARE_ACCOUNT_ID}`/`{CLOUDFLARE_GATEWAY_ID}` endpoint
    /// placeholders via [`resolve_cloudflare_base_url`].
    pub env: ProviderEnv,
    pub source: String,
}

/// Per-field merge (upstream `resolveValue`): the credential's key carries the
/// api key, its `env` map the account/gateway ids; every field falls back to
/// the ambient environment, so a credential holding only the key still picks
/// up the ids from the environment.
fn resolve_cloudflare_value(
    name: &str,
    ctx: &dyn AuthContext,
    credential: Option<&ApiKeyCredential>,
) -> AiResult<Option<String>> {
    let from_credential = match credential {
        Some(credential) => {
            if name == cloudflare::API_KEY {
                credential.key.clone()
            } else {
                credential
                    .env
                    .as_ref()
                    .and_then(|env| env.get(name).cloned())
            }
        }
        None => None,
    };
    if from_credential.is_some() {
        return Ok(from_credential);
    }
    ctx.env(name)
}

/// Resolve the Cloudflare credential (upstream `resolveCloudflareEnv`):
/// `CLOUDFLARE_API_KEY` plus `CLOUDFLARE_ACCOUNT_ID` are always required, and
/// AI Gateway additionally requires `CLOUDFLARE_GATEWAY_ID`. Returns `None`
/// when any required value is missing.
pub fn resolve_cloudflare_env(
    kind: CloudflareAuthKind,
    ctx: &dyn AuthContext,
    credential: Option<&ApiKeyCredential>,
) -> AiResult<Option<CloudflareResolvedAuth>> {
    let non_empty = |value: Option<String>| value.filter(|value| !value.trim().is_empty());
    let api_key = non_empty(resolve_cloudflare_value(
        cloudflare::API_KEY,
        ctx,
        credential,
    )?);
    let account_id = non_empty(resolve_cloudflare_value(
        cloudflare::ACCOUNT_ID,
        ctx,
        credential,
    )?);
    let gateway_id = match kind {
        CloudflareAuthKind::AiGateway => non_empty(resolve_cloudflare_value(
            cloudflare::GATEWAY_ID,
            ctx,
            credential,
        )?),
        CloudflareAuthKind::WorkersAi => None,
    };
    let (Some(api_key), Some(account_id)) = (api_key, account_id) else {
        return Ok(None);
    };
    let gateway_id = match kind {
        CloudflareAuthKind::AiGateway => match gateway_id {
            Some(gateway_id) => gateway_id,
            None => return Ok(None),
        },
        CloudflareAuthKind::WorkersAi => String::new(),
    };
    let mut env = ProviderEnv::new();
    env.insert(cloudflare::ACCOUNT_ID.to_string(), account_id);
    if kind == CloudflareAuthKind::AiGateway {
        env.insert(cloudflare::GATEWAY_ID.to_string(), gateway_id);
    }
    let source = match credential {
        Some(_) => "stored credential".to_string(),
        None => cloudflare::API_KEY.to_string(),
    };
    Ok(Some(CloudflareResolvedAuth {
        api_key,
        env,
        source,
    }))
}

/// Workers-AI auth result (upstream `cloudflareWorkersAIAuth.resolve`): the
/// key travels as the provider api key, the account id rides in the request
/// env.
pub fn cloudflare_workers_ai_auth(
    ctx: &dyn AuthContext,
    credential: Option<&ApiKeyCredential>,
) -> AiResult<Option<AuthResult>> {
    let Some(resolved) = resolve_cloudflare_env(CloudflareAuthKind::WorkersAi, ctx, credential)?
    else {
        return Ok(None);
    };
    Ok(Some(AuthResult {
        auth: ModelAuth {
            api_key: Some(resolved.api_key),
            headers: None,
            base_url: None,
        },
        env: Some(resolved.env),
        source: Some(resolved.source),
    }))
}

/// AI Gateway auth result (upstream `cloudflareAIGatewayAuth.resolve`): the
/// key rides as `cf-aig-authorization: Bearer <key>` and the account/gateway
/// ids land in the request env. Upstream also nulls `Authorization` and
/// `x-api-key` to clear the SDK's default credential headers; pir composes
/// request headers by hand, so the same clearing is realized by never emitting
/// those headers on the gateway path (see
/// `api::openai_responses::get_client_api_key`).
pub fn cloudflare_ai_gateway_auth(
    ctx: &dyn AuthContext,
    credential: Option<&ApiKeyCredential>,
) -> AiResult<Option<AuthResult>> {
    let Some(resolved) = resolve_cloudflare_env(CloudflareAuthKind::AiGateway, ctx, credential)?
    else {
        return Ok(None);
    };
    let mut headers = ProviderHeaders::new();
    headers.insert(
        "cf-aig-authorization".to_string(),
        format!("Bearer {}", resolved.api_key),
    );
    Ok(Some(AuthResult {
        auth: ModelAuth {
            api_key: None,
            headers: Some(headers),
            base_url: None,
        },
        env: Some(resolved.env),
        source: Some(resolved.source),
    }))
}

/// Replace the `{CLOUDFLARE_ACCOUNT_ID}` / `{CLOUDFLARE_GATEWAY_ID}` endpoint
/// placeholders from the resolved provider env (upstream
/// `cloudflare-stream.ts resolveCloudflareModel`); values that are missing
/// keep their placeholder. Public for tests/.
pub fn resolve_cloudflare_base_url(base_url: &str, env: &ProviderEnv) -> String {
    let mut result = base_url.to_string();
    for name in [cloudflare::ACCOUNT_ID, cloudflare::GATEWAY_ID] {
        if let Some(value) = env.get(name).filter(|value| !value.is_empty()) {
            result = result.replace(&format!("{{{name}}}"), value);
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// Resolve provider-scoped auth by inspecting the provider's api-key auth.
pub async fn resolve_provider_auth(
    provider: &dyn crate::ai::provider::Provider,
    credential: Option<&Credential>,
    ctx: &dyn AuthContext,
) -> AiResult<Option<AuthResult>> {
    let api_key_credential = match credential {
        Some(Credential::ApiKey(credential)) => Some(credential),
        _ => None,
    };
    for auth in provider.api_key_auth() {
        if let Some(result) = auth.resolve(ctx, api_key_credential)? {
            return Ok(Some(result));
        }
    }
    Ok(None)
}

/// Standard api-key auth helpers: the full upstream env-var map from
/// `packages/ai/src/env-api-keys.ts` (`getApiKeyEnvVars`).
pub mod env_api_keys {
    /// `ANTHROPIC_AUTH_TOKEN` resolves to an `Authorization: Bearer` header,
    /// not an api key (upstream `providers/anthropic.ts`).
    pub const ANTHROPIC_AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";
    /// `ANTHROPIC_OAUTH_TOKEN` also resolves to `Authorization: Bearer`.
    pub const ANTHROPIC_OAUTH_TOKEN: &str = "ANTHROPIC_OAUTH_TOKEN";

    const ANTHROPIC: &[&str] = &[
        ANTHROPIC_AUTH_TOKEN,
        ANTHROPIC_OAUTH_TOKEN,
        "ANTHROPIC_API_KEY",
    ];
    pub const OPENAI: &[&str] = &["OPENAI_API_KEY"];
    // pir keeps the legacy `GOOGLE_API_KEY` alias alongside the upstream
    // `GEMINI_API_KEY`.
    const GOOGLE: &[&str] = &["GEMINI_API_KEY", "GOOGLE_API_KEY"];
    const GOOGLE_VERTEX: &[&str] = &["GOOGLE_CLOUD_API_KEY"];
    const AZURE: &[&str] = &["AZURE_OPENAI_API_KEY"];
    const BEDROCK: &[&str] = &["AWS_BEARER_TOKEN_BEDROCK", "AWS_ACCESS_KEY_ID"];
    const XAI: &[&str] = &["XAI_API_KEY"];
    const DEEPSEEK: &[&str] = &["DEEPSEEK_API_KEY"];
    const GROQ: &[&str] = &["GROQ_API_KEY"];
    const MISTRAL: &[&str] = &["MISTRAL_API_KEY"];
    const FIREWORKS: &[&str] = &["FIREWORKS_API_KEY"];
    const TOGETHER: &[&str] = &["TOGETHER_API_KEY"];
    pub const NVIDIA: &[&str] = &["NVIDIA_API_KEY"];
    const HUGGINGFACE: &[&str] = &["HF_TOKEN"];
    const OPENROUTER: &[&str] = &["OPENROUTER_API_KEY"];
    const ANTLING: &[&str] = &["ANT_LING_API_KEY"];
    const QWEN_TOKEN_PLAN: &[&str] = &["QWEN_TOKEN_PLAN_API_KEY"];
    const QWEN_TOKEN_PLAN_CN: &[&str] = &["QWEN_TOKEN_PLAN_CN_API_KEY"];
    const QWEN_TOKEN_PLAN_INDIVIDUAL: &[&str] = &["QWEN_TOKEN_PLAN_API_KEY"];
    const CEREBRAS: &[&str] = &["CEREBRAS_API_KEY"];
    const TYPESAFE: &[&str] = &["TYPESAFE_API_KEY"];
    const RADIUS: &[&str] = &["RADIUS_API_KEY"];
    const VERCEL_AI_GATEWAY: &[&str] = &["AI_GATEWAY_API_KEY"];
    const ZAI: &[&str] = &["ZAI_API_KEY"];
    const ZAI_CODING_CN: &[&str] = &["ZAI_CODING_CN_API_KEY"];
    const MINIMAX: &[&str] = &["MINIMAX_API_KEY"];
    const MINIMAX_CN: &[&str] = &["MINIMAX_CN_API_KEY"];
    const MOONSHOTAI: &[&str] = &["MOONSHOT_API_KEY"];
    const MOONSHOTAI_CN: &[&str] = &["MOONSHOT_API_KEY"];
    const BASETEN: &[&str] = &["BASETEN_API_KEY"];
    const OPENCODE: &[&str] = &["OPENCODE_API_KEY"];
    const OPENCODE_GO: &[&str] = &["OPENCODE_API_KEY"];
    const KIMI_CODING: &[&str] = &["KIMI_API_KEY"];
    const META: &[&str] = &["META_API_KEY"];
    const CLOUDFLARE_WORKERS_AI: &[&str] = &["CLOUDFLARE_API_KEY"];
    const CLOUDFLARE_AI_GATEWAY: &[&str] = &["CLOUDFLARE_API_KEY"];
    const XIAOMI: &[&str] = &["XIAOMI_API_KEY"];
    const XIAOMI_TOKEN_PLAN_CN: &[&str] = &["XIAOMI_TOKEN_PLAN_CN_API_KEY"];
    const XIAOMI_TOKEN_PLAN_AMS: &[&str] = &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"];
    const XIAOMI_TOKEN_PLAN_SGP: &[&str] = &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"];
    const GITHUB_COPILOT: &[&str] = &["COPILOT_GITHUB_TOKEN"];

    /// Return all known provider env-var entries for status queries.
    /// Ordered by provider name.
    pub fn all() -> Vec<(&'static str, &'static [&'static str])> {
        vec![
            ("amazon-bedrock", BEDROCK),
            ("anthropic", ANTHROPIC),
            ("ant-ling", ANTLING),
            ("azure", AZURE),
            ("baseten", BASETEN),
            ("cerebras", CEREBRAS),
            ("cloudflare-ai-gateway", CLOUDFLARE_AI_GATEWAY),
            ("cloudflare-workers-ai", CLOUDFLARE_WORKERS_AI),
            ("deepseek", DEEPSEEK),
            ("fireworks", FIREWORKS),
            ("github-copilot", GITHUB_COPILOT),
            ("google", GOOGLE),
            ("google-vertex", GOOGLE_VERTEX),
            ("groq", GROQ),
            ("huggingface", HUGGINGFACE),
            ("kimi-coding", KIMI_CODING),
            ("meta", META),
            ("minimax", MINIMAX),
            ("minimax-cn", MINIMAX_CN),
            ("mistral", MISTRAL),
            ("moonshotai", MOONSHOTAI),
            ("moonshotai-cn", MOONSHOTAI_CN),
            ("nvidia", NVIDIA),
            ("openai", OPENAI),
            ("opencode", OPENCODE),
            ("opencode-go", OPENCODE_GO),
            ("openrouter", OPENROUTER),
            ("qwen-token-plan", QWEN_TOKEN_PLAN),
            ("qwen-token-plan-cn", QWEN_TOKEN_PLAN_CN),
            ("qwen-token-plan-individual", QWEN_TOKEN_PLAN_INDIVIDUAL),
            ("radius", RADIUS),
            ("together", TOGETHER),
            ("typesafe", TYPESAFE),
            ("vercel-ai-gateway", VERCEL_AI_GATEWAY),
            ("xai", XAI),
            ("xiaomi", XIAOMI),
            ("xiaomi-token-plan-ams", XIAOMI_TOKEN_PLAN_AMS),
            ("xiaomi-token-plan-cn", XIAOMI_TOKEN_PLAN_CN),
            ("xiaomi-token-plan-sgp", XIAOMI_TOKEN_PLAN_SGP),
            ("zai", ZAI),
            ("zai-coding-cn", ZAI_CODING_CN),
        ]
    }

    /// The `Authorization: Bearer` env vars for a provider (Claude Code uses
    /// `ANTHROPIC_AUTH_TOKEN` for gateway tokens and `ANTHROPIC_OAUTH_TOKEN`
    /// for OAuth access tokens; both are bearer headers, not api keys).
    pub fn bearer_env_vars(provider_id: &str) -> Vec<&'static str> {
        if provider_id == "anthropic" {
            vec![ANTHROPIC_AUTH_TOKEN, ANTHROPIC_OAUTH_TOKEN]
        } else {
            Vec::new()
        }
    }
}

impl Credential {
    /// The credential type tag.
    pub fn credential_type(&self) -> CredentialType {
        let Credential::ApiKey(_) = self;
        CredentialType::ApiKey
    }
}
