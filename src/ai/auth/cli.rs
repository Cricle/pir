//! The `pir auth` command implementations (upstream surface: `pi auth check`,
//! `pi auth print-api-key` from `cli/auth-command.ts`, plus the `/login`
//! `/logout` orchestration from `core/model-runtime.ts` exposed as
//! subcommands — pir has no interactive TUI login selector yet).

use std::collections::BTreeMap;
use std::io::BufRead;
use std::sync::Arc;

use super::interaction::{AuthEvent, AuthInteraction, AuthPrompt, LoginOptions};
use super::oauth::OAuthCredential;
use super::resolve::{login_and_store, oauth_flow_for, refresh_stored_oauth_credential};
use super::storage::{FileAuthStorage, StoredCredential};
use super::{resolve_stored_auth, stored_status, StoredAuthStatus};
use crate::ai::auth::{env_api_keys, ApiKeyAuth, EnvAuthContext};
use crate::ai::error::{AiError, AiResult};
use crate::plugins::services::{native_provider_auth_hooks, ProviderAuthError};

/// Interactive stdin/stderr login interaction (upstream's interactive-mode
/// prompts; pir reads stdin lines).
struct CliInteraction {
    opener: fn(&str),
}

impl CliInteraction {
    pub fn new() -> Self {
        Self {
            opener: super::oauth::open_browser,
        }
    }

    fn read_line() -> AiResult<String> {
        let mut line = String::new();
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let bytes = lock.read_line(&mut line).map_err(AiError::Io)?;
        if bytes == 0 {
            return Err(AiError::Authentication("Login cancelled".to_string()));
        }
        Ok(line.trim().to_string())
    }
}

impl Default for CliInteraction {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthInteraction for CliInteraction {
    fn prompt<'a>(
        &'a self,
        prompt: AuthPrompt,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<String>> + Send + 'a>> {
        Box::pin(async move {
            match prompt {
                AuthPrompt::Text { message, .. }
                | AuthPrompt::Secret { message, .. }
                | AuthPrompt::ManualCode { message, .. } => {
                    eprintln!("{message}");
                    Self::read_line()
                }
                AuthPrompt::Select { message, options } => {
                    eprintln!("{message}");
                    for (index, option) in options.iter().enumerate() {
                        eprintln!("  {}. {}", index + 1, option.label);
                    }
                    let input = Self::read_line()?;
                    let index: usize = input
                        .parse::<usize>()
                        .ok()
                        .and_then(|one_based| one_based.checked_sub(1))
                        .ok_or_else(|| {
                            AiError::Authentication(format!("invalid selection: {input}"))
                        })?;
                    options
                        .get(index)
                        .map(|option| option.id.clone())
                        .ok_or_else(|| {
                            AiError::Authentication(format!("selection out of range: {input}"))
                        })
                }
            }
        })
    }

    fn notify(&self, event: AuthEvent) {
        match event {
            AuthEvent::Info { message, links } => {
                eprintln!("{message}");
                for link in links {
                    match link.label {
                        Some(label) => eprintln!("  {label}: {}", link.url),
                        None => eprintln!("  {}", link.url),
                    }
                }
            }
            AuthEvent::AuthUrl { url, instructions } => {
                eprintln!("{url}");
                if let Some(instructions) = instructions {
                    eprintln!("{instructions}");
                }
                (self.opener)(&url);
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => {
                eprintln!("Open {verification_uri} and enter code: {user_code}");
            }
            AuthEvent::Progress { message } => {
                eprintln!("{message}");
            }
        }
    }
}

fn storage() -> FileAuthStorage {
    FileAuthStorage::default_storage()
}

/// `pir auth` / `pir auth status`: env-var and stored-credential status per
/// provider (secrets never printed).
pub fn status() -> AiResult<()> {
    let storage = storage();
    let stored = storage.list_stored()?;
    let mut stored_by_provider: BTreeMap<String, StoredAuthStatus> = BTreeMap::new();
    for (provider_id, credential_type) in stored {
        let _ = credential_type;
        stored_by_provider.insert(provider_id.clone(), stored_status(&storage, &provider_id)?);
    }

    println!("OAuth providers (pir auth login <provider>):");
    for provider in super::OAUTH_PROVIDERS {
        let state = match stored_by_provider.get(*provider) {
            Some(StoredAuthStatus::OAuth { expires }) => {
                format!("oauth (expires {expires})")
            }
            Some(StoredAuthStatus::ApiKey) => "api key".to_string(),
            _ => "not signed in".to_string(),
        };
        println!("  {provider}: {state}");
    }

    println!("\nAPI-key providers (env vars):");
    for (provider, env_vars) in env_api_keys::all() {
        let configured = env_vars.iter().any(|var| {
            std::env::var(var)
                .ok()
                .is_some_and(|value| !value.trim().is_empty())
        });
        let stored_state = match stored_by_provider.get(provider) {
            Some(StoredAuthStatus::ApiKey) => ", stored credential present",
            Some(StoredAuthStatus::OAuth { .. }) => ", stored oauth credential present",
            _ => "",
        };
        println!(
            "  {provider}: {} ({}{})",
            if configured { "set" } else { "unset" },
            env_vars.join(", "),
            stored_state
        );
    }
    Ok(())
}

/// `pir auth login <provider>`: run the provider's OAuth flow and persist the
/// credential to `~/.pi/agent/auth.json`.
pub async fn login(provider: &str, agent_name: Option<String>) -> AiResult<()> {
    let Some(flow) = oauth_flow_for(provider, None) else {
        return Err(AiError::Authentication(format!(
            "unknown OAuth provider \"{provider}\"; known: {}",
            super::OAUTH_PROVIDERS.join(", ")
        )));
    };
    let storage = storage();
    let options = LoginOptions {
        get_device_id: Some(Arc::new(|| {
            // Stable per-installation id persisted in the agent dir
            // (upstream getDeviceId contract).
            let path = crate::ai::providers::agent_dir().join("device-id");
            if let Ok(existing) = std::fs::read_to_string(&path) {
                let trimmed = existing.trim().to_string();
                if !trimmed.is_empty() {
                    return trimmed;
                }
            }
            let id = uuid::Uuid::new_v4().to_string();
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new(".")));
            let _ = std::fs::write(&path, &id);
            id
        })),
        agent_name,
    };
    let interaction = CliInteraction::new();
    let credential =
        login_and_store(&storage, provider, flow.as_ref(), &interaction, &options).await?;
    println!("Signed in to {} ({}).", flow.name(), provider);
    println!(
        "Credential stored in {} (access token redacted; expires {}).",
        storage.path().display(),
        credential.expires
    );
    Ok(())
}

/// Core logout: run the provider's native logout hook (when registered), then
/// remove the stored credential. Returns whether a credential existed.
///
/// The native hook is authoritative (it wins over any builtin/JS logout
/// path); a `Failed`/`Cancelled` result returns a typed error BEFORE the store
/// delete, so the credential stays intact. `Unavailable` falls back to the
/// plain store delete. Public for tests/ and other hosts.
pub async fn logout_stored(storage: Arc<FileAuthStorage>, provider: &str) -> AiResult<bool> {
    let existed = storage.read_stored(provider)?.is_some();
    if let Some(hooks) = native_provider_auth_hooks(provider) {
        match hooks.logout().await {
            Ok(()) | Err(ProviderAuthError::Unavailable) => {}
            Err(error) => {
                return Err(AiError::Authentication(match error {
                    ProviderAuthError::Failed(error) => {
                        format!("native provider auth logout failed for {provider}: {error}")
                    }
                    ProviderAuthError::Cancelled => {
                        format!("provider logout was cancelled for {provider}")
                    }
                    ProviderAuthError::Unavailable => unreachable!(),
                }))
            }
        }
    }
    storage.delete_async(provider).await?;
    Ok(existed)
}

/// Run one async auth operation from a synchronous caller.
///
/// `logout` is invoked from the async CLI entry point; blocking on the
/// ambient tokio runtime would panic, so the future runs on a dedicated
/// current-thread runtime on its own thread and the result is joined back.
fn run_auth_future<F, T>(future: F) -> AiResult<T>
where
    F: std::future::Future<Output = AiResult<T>> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("pir-auth-logout".to_string())
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(future),
                Err(error) => Err(AiError::Authentication(format!(
                    "failed to start the auth runtime: {error}"
                ))),
            };
            let _ = tx.send(result);
        })
        .map_err(AiError::Io)?;
    rx.recv()
        .map_err(|_| AiError::Authentication("auth logout task failed".to_string()))?
}

/// `pir auth logout <provider>`: remove the stored credential, after running
/// the native logout hook when one is registered.
pub fn logout(provider: &str) -> AiResult<()> {
    let storage = Arc::new(storage());
    let provider_id = provider.to_owned();
    let existed = {
        let storage = Arc::clone(&storage);
        run_auth_future(async move { logout_stored(storage, &provider_id).await })?
    };
    if existed {
        println!("Removed stored credential for {provider}.");
    } else {
        println!("No stored credential for {provider} (already signed out).");
    }
    Ok(())
}

fn bearer_from_headers(headers: &BTreeMap<String, String>) -> Option<String> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, value)| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
                .map(str::to_string)
        })
}

/// Resolve the printable credential for a provider, mirroring upstream
/// `getAuthCredential`: the resolved api key, or the `Authorization: Bearer`
/// value from resolved headers. OAuth credentials are refreshed when they
/// expire soon (upstream `auth check`/`print-api-key` semantics; `no_refresh`
/// skips the refresh for `auth check`).
async fn resolve_printable_credential(
    provider: &str,
    no_refresh: bool,
) -> AiResult<Option<(String, String)>> {
    let storage = Arc::new(storage());
    match storage.read_stored(provider)? {
        Some(StoredCredential::OAuth(credential)) => {
            let Some(flow) = oauth_flow_for(provider, None) else {
                return Err(AiError::Authentication(format!(
                    "no OAuth handler for stored credential of provider {provider}"
                )));
            };
            let credential = if no_refresh {
                credential
            } else {
                resolve_oauth_credential_for_print(
                    storage.clone(),
                    provider,
                    flow.clone(),
                    credential,
                )
                .await?
            };
            let auth = flow.to_auth(&credential)?;
            if let Some(api_key) = auth.api_key {
                return Ok(Some((api_key, "OAuth".to_string())));
            }
            if let Some(headers) = auth.headers {
                if let Some(bearer) = bearer_from_headers(&headers) {
                    return Ok(Some((bearer, "OAuth".to_string())));
                }
            }
            Ok(None)
        }
        Some(StoredCredential::ApiKey(credential)) => {
            let auth = ApiKeyAuth::new(provider.to_string(), Vec::new());
            let ctx = EnvAuthContext::new();
            Ok(auth
                .resolve(&ctx, Some(&credential))?
                .and_then(|result| result.auth.api_key)
                .map(|key| (key, "stored credential".to_string())))
        }
        None => {
            // Ambient env resolution.
            for (provider_id, env_vars) in env_api_keys::all() {
                if provider_id != provider {
                    continue;
                }
                for env_var in env_vars {
                    if let Ok(value) = std::env::var(env_var) {
                        if !value.trim().is_empty() {
                            return Ok(Some((value, (*env_var).to_string())));
                        }
                    }
                }
            }
            Ok(None)
        }
    }
}

async fn resolve_oauth_credential_for_print(
    storage: Arc<FileAuthStorage>,
    provider: &str,
    flow: super::OAuthFlowBox,
    credential: OAuthCredential,
) -> AiResult<OAuthCredential> {
    if !super::resolve::oauth_expires_soon(&credential, 0) {
        return Ok(credential);
    }
    refresh_stored_oauth_credential(storage, provider, flow, |current| {
        super::resolve::oauth_expires_soon(current, 0)
    })
    .await?
    .ok_or_else(|| AiError::Authentication(format!("provider {provider} logged out")))
}

/// Resolve the provider from the `--provider`/`--model` pair (upstream
/// `validateAuthCommandArgs` + `resolveCredentialForPrint`,
/// cli/auth-command.ts / cli/credential-print.ts): an explicit provider
/// wins, a model pattern resolves through the model environment, and a
/// provider+model pair requires the model to resolve under that provider.
/// `requirement` is the upstream error text for the neither-given case
/// ("Auth checks require --provider <provider> or --model <model>" /
/// "Credential printing requires --provider <provider> or --model <model>").
/// An explicit `--provider` that names no known provider errors with the
/// upstream text (credential-print.ts:57-59), and a model pattern that
/// matches several configured credentials errors with the upstream
/// ambiguity text (credential-print.ts:84-85). Public for tests/.
pub fn resolve_provider_from_flags(
    provider: Option<&str>,
    model: Option<&str>,
    requirement: &str,
) -> AiResult<String> {
    let provider = provider.map(str::trim).filter(|value| !value.is_empty());
    let model = model.map(str::trim).filter(|value| !value.is_empty());
    // Upstream unknown-provider gate: the explicit provider must name a
    // provider the runtime knows (credential-print.ts:57-59).
    if let Some(provider) = provider {
        if !known_provider(provider) {
            return Err(AiError::Authentication(format!(
                "Unknown provider \"{provider}\". Use --list-models to see available providers."
            )));
        }
    }
    match (provider, model) {
        (Some(provider), Some(model)) => {
            let resolved = provider_for_model_pattern(model)?;
            if resolved == provider {
                Ok(provider.to_string())
            } else {
                Err(AiError::Authentication(
                    "Unable to resolve the requested provider/model".to_string(),
                ))
            }
        }
        (Some(provider), None) => Ok(provider.to_string()),
        (None, Some(model)) => {
            // Upstream ambiguity gate (credential-print.ts:74-88): only
            // providers with a configured credential are candidates, and
            // several matches require `--provider`.
            let configured = configured_credential_providers();
            let matched = providers_matching_model(model, &configured)?;
            match matched.as_slice() {
                // The one configured provider that resolves the pattern wins
                // (upstream takes the credential scan's single match).
                [single] => Ok(single.clone()),
                // No configured credential resolves the pattern: keep the
                // plain pattern resolution (canonical shortcut, upstream-style
                // "Model not found" for unknown patterns).
                [] => provider_for_model_pattern(model),
                // Upstream ambiguity gate (credential-print.ts:84-85).
                multiple => Err(AiError::Authentication(format!(
                    "Multiple configured providers matched ({}). Specify --provider.",
                    multiple.join(", ")
                ))),
            }
        }
        (None, None) => Err(AiError::Authentication(requirement.to_string())),
    }
}

/// Whether a provider id is known to the runtime (upstream
/// `modelRuntime.getProvider`): a built-in catalog provider, an OAuth
/// provider, an env-key provider, or a custom `models.json` provider.
fn known_provider(provider: &str) -> bool {
    if crate::ai::providers::catalog::CATALOG_FILES
        .iter()
        .any(|(id, _)| *id == provider)
    {
        return true;
    }
    if super::OAUTH_PROVIDERS.contains(&provider) {
        return true;
    }
    if env_api_keys::all().iter().any(|(id, _)| *id == provider) {
        return true;
    }
    // Custom providers from `models.json` (fail open on load errors: the
    // credential resolution below reports the real problem).
    crate::ai::providers::load_pi_environment(None)
        .map(|environment| {
            environment
                .models
                .iter()
                .any(|model| model.provider == provider)
        })
        .unwrap_or(false)
}

/// The providers with a resolvable credential (upstream `credentialTypes`
/// from `listCredentials`): a stored `auth.json` entry or a set env API
/// key. Sorted for deterministic output.
fn configured_credential_providers() -> Vec<String> {
    let mut configured: std::collections::BTreeSet<String> = storage()
        .list_stored()
        .map(|entries| {
            entries
                .into_iter()
                .map(|(provider_id, _)| provider_id)
                .collect()
        })
        .unwrap_or_default();
    for (provider, env_vars) in env_api_keys::all() {
        if env_vars.iter().any(|env_var| {
            std::env::var(env_var)
                .ok()
                .is_some_and(|value| !value.trim().is_empty())
        }) {
            configured.insert(provider.to_string());
        }
    }
    configured.into_iter().collect()
}

/// The configured providers whose model list resolves `pattern` (upstream
/// credential-print.ts:74-88 scanning configured credentials through
/// `resolveModelReference` with an always-usable model pool). A fallback
/// custom model id (the "Using custom model id" warning) does not count as a
/// match.
fn providers_matching_model(pattern: &str, configured: &[String]) -> AiResult<Vec<String>> {
    let environment = crate::ai::providers::load_pi_environment(None)?;
    let models = environment.models.clone();
    let usable = |_: &str| true;
    let mut matched = Vec::new();
    for provider in configured {
        // Credential printing resolves each configured provider's catalog and
        // does not let stored credentials break ties (`inline_api_key`).
        let resolved = crate::ai::providers::resolution::resolve_cli_model_with_auth(
            Some(provider),
            pattern,
            &models,
            &usable,
            true,
            None,
        );
        if let Ok((Some(_), _, warning)) = resolved {
            if !warning
                .as_deref()
                .is_some_and(|warning| warning.contains("Using custom model id"))
            {
                matched.push(provider.clone());
            }
        }
    }
    Ok(matched)
}

/// The upstream `AuthCheckResult.status` values (`cli/auth-check.ts:7`).
fn emit_check_result(
    provider: &str,
    status: &str,
    reason: Option<&str>,
    auth_type: Option<&str>,
    credential: Option<&str>,
    json: bool,
) {
    if json {
        let mut object = serde_json::Map::new();
        object.insert(
            "status".to_string(),
            serde_json::Value::String(status.to_string()),
        );
        object.insert(
            "provider".to_string(),
            serde_json::Value::String(provider.to_string()),
        );
        if let Some(reason) = reason {
            object.insert(
                "reason".to_string(),
                serde_json::Value::String(reason.to_string()),
            );
        }
        if let Some(auth_type) = auth_type {
            object.insert(
                "authType".to_string(),
                serde_json::Value::String(auth_type.to_string()),
            );
        }
        if let Some(credential) = credential {
            object.insert(
                "credentials".to_string(),
                serde_json::Value::String(credential.to_string()),
            );
        }
        println!("{}", serde_json::Value::Object(object));
        return;
    }
    // Upstream prints `credential ?? result.status` (main.ts:197-199).
    println!("{}", credential.unwrap_or(status));
}

/// `pir auth check [--provider <p>] [--model <m>] [--json] [--credentials]
/// [--no-refresh]` (upstream `auth check`, `cli/auth-check.ts` + main.ts
/// 155-203): resolve the provider and report the upstream `AuthCheckResult`
/// fields (`status`/`provider`/`reason`/`authType`), with optional credential
/// material when `--credentials` is passed. Returns the upstream process exit
/// code: 0 ready, 1 not_ready, 2 invalid.
pub async fn check(
    provider: Option<&str>,
    model: Option<&str>,
    json: bool,
    credentials: bool,
    no_refresh: bool,
) -> AiResult<i32> {
    let provider_arg = provider.map(str::trim).filter(|value| !value.is_empty());
    let model_arg = model.map(str::trim).filter(|value| !value.is_empty());
    if provider_arg.is_none() && model_arg.is_none() {
        return Err(AiError::Authentication(
            "Auth checks require --provider <provider> or --model <model>".to_string(),
        ));
    }
    // Upstream resolves a model pattern first (`checkProviderAuth`,
    // cli/auth-check.ts:29-37); a resolution failure becomes
    // `{status:"invalid", reason:"invalid_state"}` at exit 2.
    let provider = match (provider_arg, model_arg) {
        (Some(provider), Some(model)) => match provider_for_model_pattern(model) {
            Ok(resolved) if resolved == provider => provider.to_string(),
            _ => {
                emit_check_result(provider, "invalid", Some("invalid_state"), None, None, json);
                return Ok(2);
            }
        },
        (Some(provider), None) => provider.to_string(),
        (None, Some(model)) => match resolve_provider_from_flags(
            None,
            Some(model),
            "Auth checks require --provider <provider> or --model <model>",
        ) {
            Ok(provider) => provider,
            Err(_) => {
                emit_check_result(model, "invalid", Some("invalid_state"), None, None, json);
                return Ok(2);
            }
        },
        (None, None) => unreachable!("guarded above"),
    };
    // Upstream's `modelRuntime.getProvider(provider)` gate: an unknown
    // provider is not_ready/provider_not_found (exit 1), not an error.
    if !known_provider(&provider) {
        emit_check_result(
            &provider,
            "not_ready",
            Some("provider_not_found"),
            None,
            None,
            json,
        );
        return Ok(1);
    }
    match resolve_printable_credential(&provider, no_refresh).await {
        Ok(Some((credential, source))) => {
            // Only the stored-OAuth flow reports "oauth"; stored api-key
            // credentials and env vars are api-key auth.
            let auth_type = if source == "OAuth" {
                "oauth"
            } else {
                "api_key"
            };
            emit_check_result(
                &provider,
                "ready",
                None,
                Some(auth_type),
                credentials.then_some(credential.as_str()),
                json,
            );
            Ok(0)
        }
        Ok(None) => {
            emit_check_result(
                &provider,
                "not_ready",
                Some("credentials_not_configured"),
                None,
                None,
                json,
            );
            Ok(1)
        }
        // A broken credential/refresh path is upstream's catch-all
        // `invalid_state` (exit 2).
        Err(_) => {
            emit_check_result(
                &provider,
                "invalid",
                Some("invalid_state"),
                None,
                None,
                json,
            );
            Ok(2)
        }
    }
}

/// `pir auth print-api-key [--provider <p>] [--model <m>]` (upstream `auth
/// print-api-key`): the provider comes from `--provider`, or resolves from
/// the `--model` pattern. A provider configured with OAuth errors with the
/// upstream text (credential-print.ts:110-112) instead of printing the
/// OAuth-derived key.
pub async fn print_api_key(provider: Option<&str>, model: Option<&str>) -> AiResult<()> {
    let provider = resolve_provider_from_flags(
        provider,
        model,
        "Credential printing requires --provider <provider> or --model <model>",
    )?;
    // Upstream skips OAuth-configured providers for the api-key kind
    // (credential-print.ts:92-94) and then reports the dedicated error
    // (credential-print.ts:110-112).
    if matches!(
        storage().read_stored(&provider)?,
        Some(StoredCredential::OAuth(_))
    ) {
        return Err(AiError::Authentication(format!(
            "Provider \"{provider}\" is configured with OAuth, not an API key"
        )));
    }
    match resolve_printable_credential(&provider, false).await? {
        Some((key, _source)) => {
            println!("{key}");
            Ok(())
        }
        None => Err(AiError::Authentication(format!(
            "no credential for provider {provider}; run `pir auth login {provider}` or set the \
             provider's API-key environment variable"
        ))),
    }
}

/// Default bearer-token minimum validity (upstream
/// `DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS` in `cli/credential-print.ts`:
/// 30 minutes).
const DEFAULT_BEARER_TOKEN_MIN_EXPIRY_SECS: u64 = 30 * 60;

/// Resolve the provider a model pattern belongs to (upstream
/// `resolveCliModel`): exact `provider/model` / bare-id match first, then a
/// fuzzy match over the configured model environment. Public for tests/.
pub fn provider_for_model_pattern(pattern: &str) -> AiResult<String> {
    if let Some((provider, model_id)) = pattern.split_once('/') {
        // A canonical `provider/model` reference needs no environment lookup.
        if !provider.trim().is_empty()
            && !model_id.trim().is_empty()
            && pattern.matches('/').count() == 1
        {
            return Ok(provider.trim().to_string());
        }
    }
    let environment = crate::ai::providers::load_pi_environment(None)?;
    let models = environment.models.clone();
    let parsed = crate::ai::providers::resolution::parse_model_pattern(pattern, &models, false);
    if let Some(model) = parsed.model {
        return Ok(model.provider);
    }
    Err(AiError::Authentication(format!(
        "Model \"{pattern}\" not found. Use --list-models to see available models."
    )))
}

/// `pir auth print-bearer-token [--provider <p>] [--model <pattern>]
/// [--min-expiry <secs>]` (upstream `pi auth print-bearer-token`,
/// `cli/credential-print.ts`): print the OAuth bearer token for the given
/// provider, or for the provider backing the given model pattern. One of
/// `--provider`/`--model` is required (upstream
/// `validateAuthCommandArgs`). Tokens expiring within `min_expiry_secs`
/// (default 30 minutes) are refreshed first.
pub async fn print_bearer_token(
    provider: Option<&str>,
    model: Option<&str>,
    min_expiry_secs: Option<u64>,
) -> crate::Result<()> {
    let provider = resolve_provider_from_flags(
        provider,
        model,
        "Credential printing requires --provider <provider> or --model <model>",
    )?;
    let storage = std::sync::Arc::new(storage());
    let Some(credential) = storage.read_stored(&provider)? else {
        return Err(anyhow::anyhow!(
            "Provider \"{provider}\" is not configured with an OAuth bearer token"
        ));
    };
    let StoredCredential::OAuth(credential) = credential else {
        return Err(anyhow::anyhow!(
            "Provider \"{provider}\" is not configured with an OAuth bearer token"
        ));
    };
    let Some(flow) = oauth_flow_for(&provider, None) else {
        return Err(anyhow::anyhow!(
            "no OAuth handler for stored credential of provider {provider}"
        ));
    };
    // Refresh when the token expires within the minimum validity window
    // (upstream getAuth's `minOAuthValidityMs` path).
    let min_expiry_ms = min_expiry_secs
        .unwrap_or(DEFAULT_BEARER_TOKEN_MIN_EXPIRY_SECS)
        .saturating_mul(1000) as i64;
    let credential = if super::resolve::oauth_expires_soon(&credential, min_expiry_ms) {
        super::resolve::refresh_stored_oauth_credential(
            storage,
            &provider,
            flow.clone(),
            move |current| super::resolve::oauth_expires_soon(current, min_expiry_ms),
        )
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("Provider \"{provider}\" is not configured with an OAuth bearer token")
        })?
    } else {
        credential
    };
    let auth = flow.to_auth(&credential)?;
    let bearer = auth
        .api_key
        .or_else(|| auth.headers.as_ref().and_then(bearer_from_headers))
        .ok_or_else(|| {
            anyhow::anyhow!("Provider \"{provider}\" is not configured with an OAuth bearer token")
        })?;
    println!("{bearer}");
    Ok(())
}

/// Shared resolution helper for tests and future callers: the registry-style
/// resolution over the file store (stored OAuth first, then stored api key,
/// then ambient env).
pub async fn resolve_provider(
    provider: &str,
    api_key_auth: Option<&ApiKeyAuth>,
) -> AiResult<Option<crate::ai::types::AuthResult>> {
    let storage = Arc::new(storage());
    let flow = oauth_flow_for(provider, None);
    resolve_stored_auth(&storage, provider, flow.as_ref(), &|credential: Option<
        &crate::ai::types::ApiKeyCredential,
    >| {
        match (api_key_auth, credential) {
            (Some(auth), _) => auth.resolve(&EnvAuthContext::new(), credential),
            (None, _) => Ok(None),
        }
    })
    .await
}
