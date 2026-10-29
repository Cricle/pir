//! MCP OAuth: discovery, the PKCE authorization-code sign-in, token
//! persistence, and refresh.
//!
//! Rust port of upstream `packages/mcp/src/oauth/{types,discovery,flow,
//! provider,callback}.ts` plus the application glue in
//! `packages/coding-agent/src/extensions/mcp/oauth.ts` (the
//! `<agent-dir>/mcp-auth.json` credential store, `signInMcpServer`, and the
//! token/refresh provider the connections use).
//!
//! The flow is an OAuth authorization code flow with PKCE S256 over a
//! loopback redirect (RFC 8252): OAuth Protected Resource Metadata
//! (RFC 9728) and Authorization Server Metadata (RFC 8414 / OpenID Connect)
//! discovery, dynamic client registration (or a Client ID Metadata
//! Document), the browser authorization with a pasted-URL fallback, and a
//! refresh-token path used before sign-in and before expiring tokens.
//! Credentials live in `<agent-dir>/mcp-auth.json`, keyed by server name and
//! URL (upstream `McpOAuthCredentialStore`).
//!
//! pir reuses its provider OAuth infrastructure instead of re-implementing
//! it: PKCE from [`crate::ai::auth::pkce`], the loopback callback server from
//! [`crate::ai::auth::callback_server`], the browser opener and the bounded
//! OAuth HTTP helpers from [`crate::ai::auth`].

use std::collections::BTreeMap;
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use super::error::McpError;
use super::registry::mcp_namespace;
use crate::ai::auth::callback_server::{query_param, CallbackWait, OAuthCallbackServer};
use crate::ai::auth::pkce::generate_pkce;
use crate::config::mcp::{McpClientRegistration, McpHttpServerConfig, McpServerConfig};

/// Loopback address the callback server binds (upstream
/// `extensions/mcp/oauth.ts:39` `CALLBACK_HOST`).
pub const DEFAULT_CALLBACK_HOST: &str = "127.0.0.1";
/// Callback route (upstream `CALLBACK_PATH`, oauth.ts:40).
pub const DEFAULT_CALLBACK_PATH: &str = "/callback";
/// Where upstream pi serves its Client ID Metadata Documents: `client.json`
/// and `<callback ID>/client.json` (upstream oauth.ts:42).
const CLIENT_METADATA_BASE_URL: &str = "https://pi.dev/oauth";
/// Redirect URI used for refreshes when none is stored (upstream oauth.ts:44).
const FALLBACK_REDIRECT_URL: &str = "http://127.0.0.1/callback";
/// Access tokens this close to expiry are refreshed before they are sent
/// (upstream oauth.ts:46 `REFRESH_SKEW_MS`).
pub const REFRESH_SKEW_MS: i64 = 30_000;
/// A refresh lock whose holder stopped renewing is taken over after this
/// (upstream oauth.ts:53).
const REFRESH_LOCK_STALE: Duration = Duration::from_secs(20);
/// How long to wait for another process's refresh (upstream oauth.ts:55).
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(25);
/// Lock retry delay (upstream oauth.ts:56).
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(100);
/// The credential-store file lock: bounded acquire with stale takeover.
const STORE_LOCK_STALE: Duration = Duration::from_secs(30);
const STORE_LOCK_WAIT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// MCP OAuth failures.
#[derive(Debug, thiserror::Error)]
pub enum McpOAuthError {
    /// An OAuth protocol error response (`error` of a token or registration
    /// answer; upstream `OAuthError` in `packages/mcp/src/oauth/errors.ts`).
    #[error("OAuth error {code}: {message}")]
    OAuth {
        /// The OAuth error code (e.g. `invalid_grant`).
        code: String,
        /// Human-readable detail.
        message: String,
    },
    /// Anything else that failed (discovery, HTTP status, parse, storage).
    #[error("{0}")]
    Fatal(String),
    /// Refusing to send credentials to a non-HTTPS endpoint (upstream
    /// `OAuthInsecureEndpointError`).
    #[error("Refusing to send OAuth credentials to non-HTTPS endpoint {0}")]
    InsecureEndpoint(String),
    /// The stored grant cannot be used; the user must sign in (upstream
    /// `McpOAuthAuthorizationRequiredError`).
    #[error("MCP OAuth authorization requires user interaction")]
    AuthorizationRequired,
    /// The sign-in was aborted or not completed in time (upstream
    /// `McpSignInCancelledError`).
    #[error("sign-in cancelled")]
    Cancelled,
}

/// Result alias for the OAuth surface.
pub type OAuthResult<T> = Result<T, McpOAuthError>;

fn fatal(message: impl Into<String>) -> McpOAuthError {
    McpOAuthError::Fatal(message.into())
}

// ---------------------------------------------------------------------------
// Wire types (upstream packages/mcp/src/oauth/types.ts)
// ---------------------------------------------------------------------------

/// The OAuth protected resource metadata document (RFC 9728; upstream
/// `OAuthProtectedResourceMetadata`, types.ts:9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OAuthProtectedResourceMetadata {
    #[serde(default)]
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_servers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, JsonValue>,
}

/// The authorization server metadata document (RFC 8414; upstream
/// `AuthorizationServerMetadata`, types.ts:16).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AuthorizationServerMetadata {
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub authorization_endpoint: String,
    #[serde(default)]
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(default)]
    pub response_types_supported: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_types_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id_metadata_document_supported: Option<bool>,
    /// Whether authorization responses carry an `iss` parameter (RFC 9207).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_response_iss_parameter_supported: Option<bool>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, JsonValue>,
}

/// The token endpoint response (upstream `OAuthTokens`, types.ts:32). Field
/// names stay snake_case: they are the protocol's wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OAuthTokens {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

/// Dynamic client registration result (upstream `OAuthClientInformationFull`,
/// types.ts:65): the client identity plus all metadata fields the server
/// echoed, preserved verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OAuthClientInformation {
    #[serde(default)]
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id_issued_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<i64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, JsonValue>,
}

impl OAuthClientInformation {
    /// The hinted token endpoint auth method (upstream
    /// `selectClientAuthMethod`'s `"token_endpoint_auth_method" in information`).
    pub fn token_endpoint_auth_method(&self) -> Option<&str> {
        self.extra
            .get("token_endpoint_auth_method")
            .and_then(JsonValue::as_str)
    }

    /// The registered redirect URIs (`redirect_uris`, default empty).
    pub fn redirect_uris(&self) -> Vec<String> {
        self.extra
            .get("redirect_uris")
            .and_then(JsonValue::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The cached discovery result (upstream `OAuthDiscoveryState`, types.ts:75).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct OAuthDiscoveryState {
    pub authorization_server_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_metadata: Option<OAuthProtectedResourceMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_metadata_url: Option<String>,
}

/// Per-server OAuth state stored in `mcp-auth.json` (upstream `McpOAuthState`,
/// extensions/mcp/oauth.ts:58 `McpOAuthState` in provider.ts:10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpOAuthState {
    pub server_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_information: Option<OAuthClientInformation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<OAuthTokens>,
    /// When the access token expires, epoch ms (upstream `tokensExpireAt`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_expire_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_verifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery: Option<OAuthDiscoveryState>,
}

/// A `WWW-Authenticate: Bearer` challenge (upstream `OAuthChallenge`,
/// types.ts:88).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OAuthChallenge {
    /// `resource_metadata` URL when it parses.
    pub resource_metadata_url: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

// ---------------------------------------------------------------------------
// Value parsing (upstream types.ts:95-209)
// ---------------------------------------------------------------------------

/// `null` and `""` count as absent: servers send them for fields they have no
/// value for (upstream `absent`, types.ts:111).
fn absent(value: Option<&JsonValue>) -> bool {
    match value {
        None | Some(JsonValue::Null) => true,
        Some(JsonValue::String(text)) => text.is_empty(),
        _ => false,
    }
}

fn json_object<'a>(
    value: &'a JsonValue,
    name: &str,
) -> OAuthResult<&'a serde_json::Map<String, JsonValue>> {
    value
        .as_object()
        .ok_or_else(|| fatal(format!("Invalid {name}")))
}

fn required_string(value: Option<&JsonValue>, name: &str) -> OAuthResult<String> {
    match value {
        Some(JsonValue::String(text)) if !text.is_empty() => Ok(text.clone()),
        _ => Err(fatal(format!("Invalid {name}"))),
    }
}

fn optional_string(value: Option<&JsonValue>, name: &str) -> OAuthResult<Option<String>> {
    if absent(value) {
        return Ok(None);
    }
    required_string(value, name).map(Some)
}

fn optional_strings(value: Option<&JsonValue>, name: &str) -> OAuthResult<Option<Vec<String>>> {
    let Some(value) = value else { return Ok(None) };
    if value.is_null() {
        return Ok(None);
    }
    let Some(items) = value.as_array() else {
        return Err(fatal(format!("Invalid {name}")));
    };
    let mut strings = Vec::with_capacity(items.len());
    for item in items {
        strings.push(required_string(Some(item), name)?);
    }
    Ok(Some(strings))
}

fn safe_url(value: Option<&JsonValue>, name: &str) -> OAuthResult<String> {
    let text = required_string(value, name)?;
    let url = reqwest::Url::parse(&text).map_err(|_| fatal(format!("Invalid {name}")))?;
    match url.scheme() {
        "javascript" | "data" | "vbscript" => Err(fatal(format!("Invalid {name}"))),
        _ => Ok(text),
    }
}

fn optional_url(value: Option<&JsonValue>, name: &str) -> OAuthResult<Option<String>> {
    if absent(value) {
        return Ok(None);
    }
    safe_url(value, name).map(Some)
}

/// Parse protected resource metadata (upstream `parseProtectedResourceMetadata`).
fn parse_protected_resource_metadata(
    value: &JsonValue,
) -> OAuthResult<OAuthProtectedResourceMetadata> {
    let input = json_object(value, "OAuth protected resource metadata")?;
    let resource = safe_url(
        input.get("resource"),
        "OAuth protected resource metadata resource",
    )?;
    let authorization_servers =
        optional_strings(input.get("authorization_servers"), "authorization_servers")?
            .map(|urls| {
                urls.into_iter()
                    .map(|url| {
                        reqwest::Url::parse(&url)
                            .map_err(|_| fatal("Invalid authorization server URL"))
                            .map(|_| url)
                    })
                    .collect::<OAuthResult<Vec<String>>>()
            })
            .transpose()?;
    let scopes_supported = optional_strings(input.get("scopes_supported"), "scopes_supported")?;
    let mut extra: BTreeMap<String, JsonValue> = BTreeMap::new();
    for (key, item) in input {
        if !matches!(
            key.as_str(),
            "resource" | "authorization_servers" | "scopes_supported"
        ) {
            extra.insert(key.clone(), item.clone());
        }
    }
    Ok(OAuthProtectedResourceMetadata {
        resource,
        authorization_servers,
        scopes_supported,
        extra,
    })
}

/// Parse authorization server metadata (upstream
/// `parseAuthorizationServerMetadata`).
fn parse_authorization_server_metadata(
    value: &JsonValue,
) -> OAuthResult<AuthorizationServerMetadata> {
    let input = json_object(value, "authorization server metadata")?;
    let response_types_supported = optional_strings(
        input.get("response_types_supported"),
        "response_types_supported",
    )?
    .ok_or_else(|| fatal("Invalid response_types_supported"))?;
    let metadata = AuthorizationServerMetadata {
        issuer: safe_url(input.get("issuer"), "authorization server issuer")?,
        authorization_endpoint: safe_url(
            input.get("authorization_endpoint"),
            "authorization endpoint",
        )?,
        token_endpoint: safe_url(input.get("token_endpoint"), "token endpoint")?,
        registration_endpoint: optional_url(
            input.get("registration_endpoint"),
            "registration endpoint",
        )?,
        scopes_supported: optional_strings(input.get("scopes_supported"), "scopes_supported")?,
        response_types_supported,
        grant_types_supported: optional_strings(
            input.get("grant_types_supported"),
            "grant_types_supported",
        )?,
        token_endpoint_auth_methods_supported: optional_strings(
            input.get("token_endpoint_auth_methods_supported"),
            "token_endpoint_auth_methods_supported",
        )?,
        code_challenge_methods_supported: optional_strings(
            input.get("code_challenge_methods_supported"),
            "code_challenge_methods_supported",
        )?,
        client_id_metadata_document_supported: input
            .get("client_id_metadata_document_supported")
            .and_then(JsonValue::as_bool),
        authorization_response_iss_parameter_supported: input
            .get("authorization_response_iss_parameter_supported")
            .and_then(JsonValue::as_bool),
        extra: BTreeMap::new(),
    };
    let mut metadata = metadata;
    for (key, item) in input {
        if !AUTHORIZATION_SERVER_METADATA_KEYS.contains(&key.as_str()) {
            metadata.extra.insert(key.clone(), item.clone());
        }
    }
    Ok(metadata)
}

const AUTHORIZATION_SERVER_METADATA_KEYS: &[&str] = &[
    "issuer",
    "authorization_endpoint",
    "token_endpoint",
    "registration_endpoint",
    "scopes_supported",
    "response_types_supported",
    "grant_types_supported",
    "token_endpoint_auth_methods_supported",
    "code_challenge_methods_supported",
    "client_id_metadata_document_supported",
    "authorization_response_iss_parameter_supported",
];

/// Parse a token endpoint response (upstream `parseOAuthTokens`): unknown
/// fields are dropped, `null`/`""` count as absent, and `expires_in` coerces
/// numbers and numeric strings.
fn parse_oauth_tokens(value: &JsonValue) -> OAuthResult<OAuthTokens> {
    let input = json_object(value, "OAuth token response")?;
    let expires = match input.get("expires_in") {
        Some(item) if !absent(Some(item)) => {
            let number = item.as_f64().or_else(|| {
                item.as_str()
                    .and_then(|text| text.trim().parse::<f64>().ok())
            });
            match number {
                Some(number) if number.is_finite() => Some(number as i64),
                _ => return Err(fatal("Invalid expires_in")),
            }
        }
        _ => None,
    };
    Ok(OAuthTokens {
        access_token: required_string(input.get("access_token"), "access_token")?,
        token_type: required_string(input.get("token_type"), "token_type")?,
        expires_in: expires,
        scope: optional_string(input.get("scope"), "scope")?,
        refresh_token: optional_string(input.get("refresh_token"), "refresh_token")?,
        id_token: optional_string(input.get("id_token"), "id_token")?,
    })
}

/// Parse a dynamic client registration response (upstream
/// `parseClientInformation`).
fn parse_client_information(value: &JsonValue) -> OAuthResult<OAuthClientInformation> {
    let input = json_object(value, "OAuth client registration response")?;
    let redirect_uris =
        optional_strings(input.get("redirect_uris"), "redirect_uris")?.unwrap_or_default();
    let mut extra: BTreeMap<String, JsonValue> = BTreeMap::new();
    for (key, item) in input {
        if !matches!(
            key.as_str(),
            "client_id" | "client_secret" | "client_id_issued_at" | "client_secret_expires_at"
        ) {
            extra.insert(key.clone(), item.clone());
        }
    }
    extra.insert(
        "redirect_uris".to_string(),
        JsonValue::Array(redirect_uris.into_iter().map(JsonValue::String).collect()),
    );
    Ok(OAuthClientInformation {
        client_id: required_string(input.get("client_id"), "client_id")?,
        client_secret: optional_string(input.get("client_secret"), "client_secret")?,
        client_id_issued_at: input.get("client_id_issued_at").and_then(JsonValue::as_i64),
        client_secret_expires_at: input
            .get("client_secret_expires_at")
            .and_then(JsonValue::as_i64),
        extra,
    })
}

// ---------------------------------------------------------------------------
// WWW-Authenticate (upstream discovery.ts:33-56)
// ---------------------------------------------------------------------------

fn header_field_regex() -> Option<&'static regex::Regex> {
    static FIELD: OnceLock<Option<regex::Regex>> = OnceLock::new();
    FIELD
        .get_or_init(|| {
            regex::Regex::new(r#"(?i)(?:^|[,\s])([A-Za-z_][A-Za-z0-9_-]*)=(?:"([^"]*)"|([^\s,]+))"#)
                .ok()
        })
        .as_ref()
}

/// One `name=value` parameter of a `WWW-Authenticate` header; an empty quoted
/// value carries no information and counts as absent.
fn www_authenticate_field(header: &str, name: &str) -> Option<String> {
    let regex = header_field_regex()?;
    for captures in regex.captures_iter(header) {
        let Some(key) = captures.get(1) else { continue };
        if !key.as_str().eq_ignore_ascii_case(name) {
            continue;
        }
        let quoted = captures
            .get(2)
            .map(|value| value.as_str())
            .filter(|value| !value.is_empty());
        if let Some(quoted) = quoted {
            return Some(quoted.to_string());
        }
        if let Some(bare) = captures.get(3) {
            return Some(bare.as_str().to_string());
        }
    }
    None
}

/// Parse a `WWW-Authenticate: Bearer|DPoP` header (upstream
/// `parseWwwAuthenticate`; other schemes yield an empty challenge).
pub fn parse_www_authenticate(header: Option<&str>) -> OAuthChallenge {
    let Some(header) = header else {
        return OAuthChallenge::default();
    };
    let scheme = header
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if scheme != "bearer" && scheme != "dpop" {
        return OAuthChallenge::default();
    }
    let resource_metadata = www_authenticate_field(header, "resource_metadata")
        .filter(|value| reqwest::Url::parse(value).is_ok());
    OAuthChallenge {
        resource_metadata_url: resource_metadata,
        scope: www_authenticate_field(header, "scope"),
        error: www_authenticate_field(header, "error"),
        error_description: www_authenticate_field(header, "error_description"),
    }
}

/// Scopes for a step-up authorization: the challenged scopes plus the ones
/// granted so far (upstream `stepUpScope`, flow.ts:314).
pub fn step_up_scope(granted: Option<&str>, challenged: Option<&str>) -> Option<String> {
    let challenged = challenged?;
    let mut scopes: Vec<&str> = Vec::new();
    for scope in [granted, Some(challenged)].into_iter().flatten() {
        for item in scope.split_whitespace().filter(|item| !item.is_empty()) {
            if !scopes.contains(&item) {
                scopes.push(item);
            }
        }
    }
    if scopes.is_empty() {
        None
    } else {
        Some(scopes.join(" "))
    }
}

// ---------------------------------------------------------------------------
// Discovery (upstream packages/mcp/src/oauth/discovery.ts)
// ---------------------------------------------------------------------------

/// Path suffix for `/.well-known/<kind><path>`; empty for the root path
/// (upstream `pathSuffix`).
fn path_suffix(pathname: &str) -> &str {
    pathname.strip_suffix('/').unwrap_or(pathname)
}

/// 4xx and 502 mean "not here", so discovery tries the next candidate URL
/// (upstream `isDiscoveryMiss`).
fn is_discovery_miss(status: u16) -> bool {
    (400..500).contains(&status) || status == 502
}

async fn fetch_metadata(url: &str) -> OAuthResult<(u16, String)> {
    let response = crate::ai::auth::http::get_json(
        url,
        None,
        &[(
            "MCP-Protocol-Version",
            super::protocol::LATEST_PROTOCOL_VERSION,
        )],
    )
    .await
    .map_err(|error| fatal(error.to_string()))?;
    Ok((response.status, response.body))
}

/// Discover the protected resource metadata (upstream
/// `discoverProtectedResourceMetadata`, discovery.ts:70).
async fn discover_protected_resource_metadata(
    server_url: &reqwest::Url,
    resource_metadata_url: Option<&str>,
) -> OAuthResult<OAuthProtectedResourceMetadata> {
    let origin = server_url.origin().ascii_serialization();
    let (status, body) = match resource_metadata_url {
        Some(explicit) => fetch_metadata(explicit).await?,
        None => {
            let path = path_suffix(server_url.path());
            let url = format!("{origin}/.well-known/oauth-protected-resource{path}");
            let (status, body) = fetch_metadata(&url).await?;
            // A miss on a server-specific path may just mean the resource
            // only serves the root well-known document.
            if server_url.path() != "/" && is_discovery_miss(status) {
                fetch_metadata(&format!("{origin}/.well-known/oauth-protected-resource")).await?
            } else {
                (status, body)
            }
        }
    };
    if !(200..300).contains(&status) {
        return Err(fatal(format!(
            "HTTP {status} loading OAuth protected resource metadata"
        )));
    }
    parse_protected_resource_metadata(&serde_json::from_str(&body).map_err(|error| {
        fatal(format!(
            "Invalid OAuth protected resource metadata: {error}"
        ))
    })?)
}

/// Candidate metadata URLs for an authorization server (upstream
/// `buildAuthorizationServerDiscoveryUrls`).
fn build_authorization_server_discovery_urls(
    authorization_server_url: &reqwest::Url,
) -> Vec<String> {
    let issuer = authorization_server_url;
    let origin = issuer.origin().ascii_serialization();
    let path = path_suffix(issuer.path());
    let mut urls = vec![
        format!("{origin}/.well-known/oauth-authorization-server{path}"),
        format!("{origin}/.well-known/openid-configuration{path}"),
    ];
    if !path.is_empty() {
        urls.push(format!("{origin}{path}/.well-known/openid-configuration"));
    }
    urls
}

/// Discover authorization server metadata; `Ok(None)` when every candidate is
/// a miss (upstream `discoverAuthorizationServerMetadata`).
async fn discover_authorization_server_metadata(
    authorization_server_url: &reqwest::Url,
    skip_issuer_validation: bool,
) -> OAuthResult<Option<AuthorizationServerMetadata>> {
    for url in build_authorization_server_discovery_urls(authorization_server_url) {
        let (status, body) = fetch_metadata(&url).await?;
        if !(200..300).contains(&status) {
            if is_discovery_miss(status) {
                continue;
            }
            return Err(fatal(format!(
                "HTTP {status} loading authorization server metadata from {url}"
            )));
        }
        let metadata =
            parse_authorization_server_metadata(&serde_json::from_str(&body).map_err(
                |error| fatal(format!("Invalid authorization server metadata: {error}")),
            )?)?;
        if !skip_issuer_validation {
            let expected = authorization_server_url.to_string();
            let trim = |value: &str| value.strip_suffix('/').unwrap_or(value).to_string();
            if trim(&metadata.issuer) != trim(&expected) {
                return Err(fatal(format!(
                    "OAuth issuer mismatch: expected {:?}, received {:?}",
                    expected, metadata.issuer
                )));
            }
        }
        return Ok(Some(metadata));
    }
    Ok(None)
}

/// The result of protected-resource + authorization-server discovery
/// (upstream `OAuthServerInfo`).
#[derive(Debug, Clone)]
struct OAuthServerInfo {
    authorization_server_url: reqwest::Url,
    authorization_server_metadata: Option<AuthorizationServerMetadata>,
    resource_metadata: Option<OAuthProtectedResourceMetadata>,
}

/// Discover the authorization server for `server_url` (upstream
/// `discoverOAuthServerInfo`). A protected-resource-metadata failure is
/// tolerated: the server URL's origin is used as the authorization server.
async fn discover_oauth_server_info(
    server_url: &reqwest::Url,
    resource_metadata_url: Option<&str>,
    authorization_server_metadata_url: Option<&str>,
) -> OAuthResult<OAuthServerInfo> {
    let resource_metadata = discover_protected_resource_metadata(server_url, resource_metadata_url)
        .await
        .ok();
    if let Some(metadata_url) = authorization_server_metadata_url {
        let (status, body) = fetch_metadata(metadata_url).await?;
        if !(200..300).contains(&status) {
            return Err(fatal(format!(
                "HTTP {status} loading authorization server metadata from {metadata_url}"
            )));
        }
        let metadata =
            parse_authorization_server_metadata(&serde_json::from_str(&body).map_err(
                |error| fatal(format!("Invalid authorization server metadata: {error}")),
            )?)?;
        let authorization_server_url = reqwest::Url::parse(&metadata.issuer)
            .map_err(|error| fatal(format!("Invalid authorization server issuer: {error}")))?;
        return Ok(OAuthServerInfo {
            authorization_server_url,
            authorization_server_metadata: Some(metadata),
            resource_metadata,
        });
    }
    let authorization_server_url = match resource_metadata
        .as_ref()
        .and_then(|metadata| metadata.authorization_servers.as_ref())
        .and_then(|servers| servers.first())
    {
        Some(first) => reqwest::Url::parse(first)
            .map_err(|error| fatal(format!("Invalid authorization server URL: {error}")))?,
        None => server_url
            .join("/")
            .map_err(|error| fatal(format!("Invalid MCP server URL: {error}")))?,
    };
    let metadata = discover_authorization_server_metadata(&authorization_server_url, false).await?;
    Ok(OAuthServerInfo {
        authorization_server_url,
        authorization_server_metadata: metadata,
        resource_metadata,
    })
}

/// The `resource` the protected resource metadata advertises for `server_url`
/// (upstream `selectResource`): same origin, configured path is a prefix.
fn select_resource(
    server_url: &reqwest::Url,
    metadata: Option<&OAuthProtectedResourceMetadata>,
) -> OAuthResult<Option<String>> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    let mut requested = server_url.clone();
    requested.set_fragment(None);
    let configured = reqwest::Url::parse(&metadata.resource)
        .map_err(|error| fatal(format!("Invalid protected resource URL: {error}")))?;
    if requested.origin() != configured.origin() {
        return Err(fatal(format!(
            "Protected resource {} does not match MCP server {requested}",
            metadata.resource
        )));
    }
    let requested_path = format!("{}/", path_suffix(requested.path()));
    let configured_path = format!("{}/", path_suffix(configured.path()));
    if !requested_path.starts_with(&configured_path) {
        return Err(fatal(format!(
            "Protected resource {} does not match MCP server {requested}",
            metadata.resource
        )));
    }
    Ok(Some(metadata.resource.clone()))
}

// ---------------------------------------------------------------------------
// Flow (upstream packages/mcp/src/oauth/flow.ts)
// ---------------------------------------------------------------------------

/// Refuse to send OAuth credentials to a non-HTTPS endpoint (upstream
/// `secureEndpoint`; loopback is exempt, RFC 8252).
fn secure_endpoint(value: &str) -> OAuthResult<reqwest::Url> {
    let url = reqwest::Url::parse(value)
        .map_err(|error| fatal(format!("Invalid OAuth endpoint {value}: {error}")))?;
    let loopback_host = matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("[::1]") | Some("::1")
    ) || url
        .host_str()
        .and_then(|host| {
            host.trim_matches(|ch| ch == '[' || ch == ']')
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|ip| ip.is_loopback());
    if url.scheme() != "https" && !loopback_host {
        return Err(McpOAuthError::InsecureEndpoint(url.to_string()));
    }
    Ok(url)
}

/// `native` for loopback or custom-scheme redirect URIs, else `web` (MCP
/// SEP-837; upstream `applicationType`).
fn application_type(redirect_uris: &[String]) -> &'static str {
    let native = redirect_uris.iter().any(|uri| {
        let Ok(url) = reqwest::Url::parse(uri) else {
            return false;
        };
        let loopback = matches!(
            url.host_str(),
            Some("localhost") | Some("127.0.0.1") | Some("[::1]") | Some("::1")
        );
        (url.scheme() != "http" && url.scheme() != "https") || loopback
    });
    if native {
        "native"
    } else {
        "web"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientAuthMethod {
    ClientSecretBasic,
    ClientSecretPost,
    None,
}

/// Pick the token endpoint client auth method (upstream
/// `selectClientAuthMethod`).
fn select_client_auth_method(
    information: &OAuthClientInformation,
    supported: &[String],
) -> ClientAuthMethod {
    let supported_has = |method: &str| supported.iter().any(|item| item == method);
    if let Some(hinted) = information.token_endpoint_auth_method() {
        let known = matches!(
            hinted,
            "client_secret_basic" | "client_secret_post" | "none"
        );
        if known && (supported.is_empty() || supported_has(hinted)) {
            return match hinted {
                "client_secret_basic" => ClientAuthMethod::ClientSecretBasic,
                "client_secret_post" => ClientAuthMethod::ClientSecretPost,
                _ => ClientAuthMethod::None,
            };
        }
    }
    if supported.is_empty() {
        return if information.client_secret.is_some() {
            ClientAuthMethod::ClientSecretBasic
        } else {
            ClientAuthMethod::None
        };
    }
    if information.client_secret.is_some() && supported_has("client_secret_basic") {
        return ClientAuthMethod::ClientSecretBasic;
    }
    if information.client_secret.is_some() && supported_has("client_secret_post") {
        return ClientAuthMethod::ClientSecretPost;
    }
    if supported_has("none") {
        return ClientAuthMethod::None;
    }
    if information.client_secret.is_some() {
        ClientAuthMethod::ClientSecretPost
    } else {
        ClientAuthMethod::None
    }
}

/// The client auth applied to a token request (upstream
/// `applyClientAuthentication`).
fn apply_client_authentication(
    method: ClientAuthMethod,
    information: &OAuthClientInformation,
    headers: &mut Vec<(String, String)>,
    params: &mut Vec<(String, String)>,
) -> OAuthResult<()> {
    match method {
        ClientAuthMethod::ClientSecretBasic => {
            let Some(secret) = information.client_secret.as_deref() else {
                return Err(fatal("client_secret_basic requires a client secret"));
            };
            use base64::Engine as _;
            let credentials = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{secret}", information.client_id));
            headers.push(("Authorization".to_string(), format!("Basic {credentials}")));
        }
        ClientAuthMethod::ClientSecretPost => {
            params.push(("client_id".to_string(), information.client_id.clone()));
            if let Some(secret) = information.client_secret.as_deref() {
                params.push(("client_secret".to_string(), secret.to_string()));
            }
        }
        ClientAuthMethod::None => {
            params.push(("client_id".to_string(), information.client_id.clone()));
        }
    }
    Ok(())
}

/// An authorization request plus its PKCE verifier (upstream
/// `startAuthorization`).
struct AuthorizationStart {
    authorization_url: reqwest::Url,
    code_verifier: String,
}

/// Build the authorization URL with PKCE S256 (upstream `pkce` +
/// `startAuthorization`; the PKCE pair comes from pir's provider flows).
fn start_authorization(
    authorization_server_url: &reqwest::Url,
    metadata: Option<&AuthorizationServerMetadata>,
    client_information: &OAuthClientInformation,
    redirect_url: &str,
    scope: Option<&str>,
    state: Option<&str>,
    resource: Option<&str>,
) -> OAuthResult<AuthorizationStart> {
    if let Some(metadata) = metadata {
        if !metadata
            .response_types_supported
            .iter()
            .any(|item| item == "code")
        {
            return Err(fatal(
                "Authorization server does not support authorization codes",
            ));
        }
        if let Some(methods) = &metadata.code_challenge_methods_supported {
            if !methods.iter().any(|item| item == "S256") {
                return Err(fatal("Authorization server does not support PKCE S256"));
            }
        }
    }
    let mut url = match metadata.map(|metadata| metadata.authorization_endpoint.as_str()) {
        Some(endpoint) => reqwest::Url::parse(endpoint)
            .map_err(|error| fatal(format!("Invalid authorization endpoint: {error}")))?,
        None => authorization_server_url
            .join("/authorize")
            .map_err(|error| fatal(format!("Invalid authorization server URL: {error}")))?,
    };
    let pkce = generate_pkce();
    let mut pairs: Vec<(String, String)> = vec![
        ("response_type".to_string(), "code".to_string()),
        (
            "client_id".to_string(),
            client_information.client_id.clone(),
        ),
        ("code_challenge".to_string(), pkce.challenge),
        ("code_challenge_method".to_string(), "S256".to_string()),
        ("redirect_uri".to_string(), redirect_url.to_string()),
    ];
    if let Some(state) = state {
        pairs.push(("state".to_string(), state.to_string()));
    }
    if let Some(scope) = scope.filter(|scope| !scope.is_empty()) {
        pairs.push(("scope".to_string(), scope.to_string()));
        if scope
            .split_whitespace()
            .any(|item| item == "offline_access")
        {
            pairs.push(("prompt".to_string(), "consent".to_string()));
        }
    }
    if let Some(resource) = resource {
        pairs.push(("resource".to_string(), resource.to_string()));
    }
    set_query_params(&mut url, &pairs);
    Ok(AuthorizationStart {
        authorization_url: url,
        code_verifier: pkce.verifier,
    })
}

/// `searchParams.set` semantics: replace an existing parameter, keep the
/// others.
fn set_query_params(url: &mut reqwest::Url, pairs: &[(String, String)]) {
    let existing: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.query_pairs_mut().clear();
    for (key, value) in existing {
        if !pairs.iter().any(|(name, _)| name == &key) {
            url.query_pairs_mut().append_pair(&key, &value);
        }
    }
    for (key, value) in pairs {
        url.query_pairs_mut().append_pair(key, value);
    }
}

/// Register a client dynamically (upstream `registerClient`).
async fn register_client(
    authorization_server_url: &reqwest::Url,
    metadata: Option<&AuthorizationServerMetadata>,
    client_metadata: &JsonValue,
    scope: Option<&str>,
) -> OAuthResult<OAuthClientInformation> {
    let endpoint = metadata.and_then(|metadata| metadata.registration_endpoint.clone());
    if metadata.is_some() && endpoint.is_none() {
        return Err(fatal(
            "Authorization server does not support dynamic client registration",
        ));
    }
    let url = match endpoint {
        Some(endpoint) => endpoint,
        None => authorization_server_url
            .join("/register")
            .map_err(|error| fatal(format!("Invalid authorization server URL: {error}")))?
            .to_string(),
    };
    let mut body = client_metadata.clone();
    if let Some(object) = body.as_object_mut() {
        if !object.contains_key("application_type") {
            let redirect_uris: Vec<String> = object
                .get("redirect_uris")
                .and_then(JsonValue::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            object.insert(
                "application_type".to_string(),
                JsonValue::String(application_type(&redirect_uris).to_string()),
            );
        }
        if let Some(scope) = scope.filter(|scope| !scope.is_empty()) {
            object.insert("scope".to_string(), JsonValue::String(scope.to_string()));
        }
    }
    let response = crate::ai::auth::http::post_json_with_headers(&url, &body, &[])
        .await
        .map_err(|error| fatal(error.to_string()))?;
    if !(200..300).contains(&response.status) {
        return Err(fatal(format!(
            "OAuth dynamic client registration failed with status {}: {}",
            response.status, response.body
        )));
    }
    let value: JsonValue = serde_json::from_str(&response.body).map_err(|error| {
        fatal(format!(
            "Invalid OAuth client registration response: {error}"
        ))
    })?;
    parse_client_information(&value)
}

/// A token endpoint request's options (upstream `TokenRequestOptions`).
struct TokenRequestOptions<'a> {
    metadata: Option<&'a AuthorizationServerMetadata>,
    client_information: &'a OAuthClientInformation,
    resource: Option<&'a str>,
}

/// POST the token endpoint (upstream `tokenRequest`): OAuth errors are read
/// from the body before the status, and `client_secret_basic` / `post` /
/// `none` client authentication is applied.
async fn token_request(
    authorization_server_url: &reqwest::Url,
    options: &TokenRequestOptions<'_>,
    mut params: Vec<(String, String)>,
) -> OAuthResult<OAuthTokens> {
    let endpoint = match options
        .metadata
        .map(|metadata| metadata.token_endpoint.as_str())
    {
        Some(endpoint) => endpoint.to_string(),
        None => authorization_server_url
            .join("/token")
            .map_err(|error| fatal(format!("Invalid authorization server URL: {error}")))?
            .to_string(),
    };
    let url = secure_endpoint(&endpoint)?;
    if let Some(resource) = options.resource {
        params.push(("resource".to_string(), resource.to_string()));
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    let method = select_client_auth_method(
        options.client_information,
        options
            .metadata
            .and_then(|metadata| metadata.token_endpoint_auth_methods_supported.as_deref())
            .unwrap_or(&[]),
    );
    apply_client_authentication(
        method,
        options.client_information,
        &mut headers,
        &mut params,
    )?;
    let fields: Vec<(&str, &str)> = params
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let header_refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let response =
        crate::ai::auth::http::post_form_with_headers(url.as_str(), &fields, &header_refs)
            .await
            .map_err(|error| fatal(error.to_string()))?;
    let value: Option<JsonValue> = serde_json::from_str(&response.body).ok();
    if let Some(JsonValue::Object(object)) = &value {
        if let Some(code) = object.get("error").and_then(JsonValue::as_str) {
            let message = object
                .get("error_description")
                .and_then(JsonValue::as_str)
                .unwrap_or(code)
                .to_string();
            return Err(McpOAuthError::OAuth {
                code: code.to_string(),
                message,
            });
        }
    }
    if !(200..300).contains(&response.status) {
        return Err(McpOAuthError::OAuth {
            code: "server_error".to_string(),
            message: format!("HTTP {}: {}", response.status, response.body),
        });
    }
    parse_oauth_tokens(value.as_ref().unwrap_or(&JsonValue::Null))
}

/// Exchange an authorization code (upstream `exchangeAuthorizationCode`).
async fn exchange_authorization_code(
    authorization_server_url: &reqwest::Url,
    options: &TokenRequestOptions<'_>,
    code: &str,
    code_verifier: &str,
    redirect_url: &str,
) -> OAuthResult<OAuthTokens> {
    token_request(
        authorization_server_url,
        options,
        vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code.to_string()),
            ("code_verifier".to_string(), code_verifier.to_string()),
            ("redirect_uri".to_string(), redirect_url.to_string()),
        ],
    )
    .await
}

/// Refresh the tokens; a refresh answer without `refresh_token` keeps the old
/// one (upstream `refreshAuthorization`).
async fn refresh_authorization(
    authorization_server_url: &reqwest::Url,
    options: &TokenRequestOptions<'_>,
    refresh_token: &str,
) -> OAuthResult<OAuthTokens> {
    let mut tokens = token_request(
        authorization_server_url,
        options,
        vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
        ],
    )
    .await?;
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh_token.to_string());
    }
    Ok(tokens)
}

/// Record the requested scope when the server's answer carries none
/// (upstream `withScope`).
fn with_scope(mut tokens: OAuthTokens, scope: Option<&str>) -> OAuthTokens {
    if tokens.scope.is_none() {
        if let Some(scope) = scope.filter(|scope| !scope.is_empty()) {
            tokens.scope = Some(scope.to_string());
        }
    }
    tokens
}

/// A Client ID Metadata Document to identify as instead of registering
/// (upstream `OAuthClientMetadataDocument`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClientMetadataDocument {
    url: String,
    redirect_url: String,
}

/// 12 characters identifying an MCP server URL in callback paths (upstream
/// `callbackId`, oauth.ts:239).
fn callback_id(server_url: &str) -> String {
    let mut normalized = server_url.to_string();
    if let Ok(mut url) = reqwest::Url::parse(server_url) {
        url.set_fragment(None);
        normalized = url.to_string();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(normalized.as_bytes());
    crate::ai::auth::pkce::base64url_encode(&digest[..9])
}

/// What the flow may do to this server's stored credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialKind {
    All,
    Tokens,
}

/// The flow's view of one server's credentials (upstream `McpOAuthProvider`):
/// a configured client wins over stored state, and everything is keyed by the
/// exact server URL.
struct FlowProvider<'a> {
    server_url: String,
    redirect_url: String,
    client_metadata: JsonValue,
    /// `Some` when the configuration selects Client ID Metadata Documents.
    client_metadata_document: Option<McpClientRegistration>,
    configured_client: Option<OAuthClientInformation>,
    store: &'a McpOAuthServerStore,
}

impl FlowProvider<'_> {
    async fn state(&self) -> OAuthResult<String> {
        if let Some(existing) = self
            .store
            .load()
            .await?
            .and_then(|state| state.oauth_state)
            .filter(|state| !state.is_empty())
        {
            return Ok(existing);
        }
        let state = random_state();
        let generated = state.clone();
        self.store
            .update(|stored| stored.oauth_state = Some(generated))
            .await?;
        Ok(state)
    }

    async fn client_information(&self) -> OAuthResult<Option<OAuthClientInformation>> {
        if self.configured_client.is_some() {
            return Ok(self.configured_client.clone());
        }
        Ok(self
            .store
            .load()
            .await?
            .and_then(|state| state.client_information))
    }

    async fn save_client_information(
        &self,
        information: &OAuthClientInformation,
    ) -> OAuthResult<()> {
        if self.configured_client.is_some() {
            return Ok(());
        }
        let information = information.clone();
        self.store
            .update(|state| state.client_information = Some(information))
            .await
    }

    async fn tokens(&self) -> OAuthResult<Option<OAuthTokens>> {
        Ok(self.store.load().await?.and_then(|state| state.tokens))
    }

    async fn save_tokens(&self, tokens: OAuthTokens) -> OAuthResult<()> {
        let expires_at = tokens
            .expires_in
            .map(|expires| now_ms().saturating_add(expires.saturating_mul(1000)));
        self.store
            .update(|state| {
                state.tokens = Some(tokens);
                state.tokens_expire_at = expires_at;
            })
            .await
    }

    async fn save_code_verifier(&self, verifier: &str) -> OAuthResult<()> {
        let verifier = verifier.to_string();
        self.store
            .update(|state| state.code_verifier = Some(verifier))
            .await
    }

    async fn code_verifier(&self) -> OAuthResult<String> {
        self.store
            .load()
            .await?
            .and_then(|state| state.code_verifier)
            .filter(|verifier| !verifier.is_empty())
            .ok_or_else(|| fatal("No OAuth PKCE code verifier is stored"))
    }

    async fn invalidate_credentials(&self, kind: CredentialKind) -> OAuthResult<()> {
        self.store
            .update(|state| match kind {
                CredentialKind::All => {
                    state.client_information = None;
                    state.tokens = None;
                    state.tokens_expire_at = None;
                    state.code_verifier = None;
                    state.discovery = None;
                    state.oauth_state = None;
                }
                CredentialKind::Tokens => {
                    state.tokens = None;
                    state.tokens_expire_at = None;
                }
            })
            .await
    }

    async fn save_discovery_state(&self, discovery: OAuthDiscoveryState) -> OAuthResult<()> {
        self.store
            .update(|state| state.discovery = Some(discovery))
            .await
    }

    /// The Client ID Metadata Document to identify as (upstream
    /// `clientMetadataDocument`, oauth.ts:251).
    fn client_metadata_document(
        &self,
        metadata: Option<&AuthorizationServerMetadata>,
    ) -> OAuthResult<Option<ClientMetadataDocument>> {
        if self.client_metadata_document != Some(McpClientRegistration::Cimd) {
            return Ok(None);
        }
        let metadata = metadata.ok_or_else(|| {
            fatal("The authorization server metadata is required for Client ID Metadata Documents")
        })?;
        if metadata.client_id_metadata_document_supported != Some(true)
            || !metadata
                .token_endpoint_auth_methods_supported
                .as_ref()
                .is_some_and(|methods| methods.iter().any(|method| method == "none"))
        {
            return Err(fatal(
                "The authorization server does not support Client ID Metadata Documents for public clients; remove oauth.clientRegistration \"cimd\"",
            ));
        }
        if metadata.authorization_response_iss_parameter_supported == Some(true) {
            return Ok(Some(ClientMetadataDocument {
                url: format!("{CLIENT_METADATA_BASE_URL}/client.json"),
                redirect_url: self.redirect_url.clone(),
            }));
        }
        let id = callback_id(&self.server_url);
        let mut redirect = reqwest::Url::parse(&self.redirect_url)
            .map_err(|error| fatal(format!("Invalid redirect URI: {error}")))?;
        redirect.set_path(&format!("{DEFAULT_CALLBACK_PATH}/{id}"));
        Ok(Some(ClientMetadataDocument {
            url: format!("{CLIENT_METADATA_BASE_URL}/{id}/client.json"),
            redirect_url: redirect.to_string(),
        }))
    }
}

fn now_ms() -> i64 {
    crate::ai::util::now_ms() as i64
}

/// 32 random bytes hex-encoded (upstream `provider.state()`).
fn random_state() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Options of one `run_flow` pass (upstream `OAuthFlowOptions`).
#[derive(Default)]
struct FlowOptions<'a> {
    authorization_code: Option<&'a str>,
    iss: Option<&'a str>,
    scope: Option<String>,
    resource_metadata_url: Option<&'a str>,
    authorization_server_metadata_url: Option<&'a str>,
    skip_refresh: bool,
}

/// The outcome of one flow pass (upstream `OAuthFlowResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlowResult {
    Authorized,
    Redirect,
}

/// The RFC 9207 `iss` check on an authorization response, as the upstream
/// flow performs it (`flow.ts:385-388`):
///
/// - an `iss` parameter is always checked; when no authorization server
///   metadata was discovered there is no issuer to compare with, so any
///   presented `iss` is a mismatch (`expected` reports `none`);
/// - a missing `iss` is only a mismatch when the server promised the
///   parameter (`authorization_response_iss_parameter_supported`).
///
/// Returns the upstream error text, or `None` when the code may be exchanged.
pub fn authorization_issuer_mismatch(
    metadata: Option<&AuthorizationServerMetadata>,
    iss: Option<&str>,
) -> Option<String> {
    let must_check = iss.is_some()
        || metadata.and_then(|metadata| metadata.authorization_response_iss_parameter_supported)
            == Some(true);
    if !must_check || iss == metadata.map(|metadata| metadata.issuer.as_str()) {
        return None;
    }
    let format = |issuer: Option<&str>| {
        issuer
            .map(|issuer| format!("{issuer:?}"))
            .unwrap_or_else(|| "none".to_string())
    };
    Some(format!(
        "OAuth issuer mismatch: expected {}, received {}",
        format(metadata.map(|metadata| metadata.issuer.as_str())),
        format(iss)
    ))
}

/// One authorization pass (upstream `runFlow`): discovery, client
/// registration or reuse, then either a code exchange, a refresh, or the
/// authorization redirect handed to `on_redirect`.
async fn run_flow(
    provider: &FlowProvider<'_>,
    server_url: &reqwest::Url,
    options: &FlowOptions<'_>,
    on_redirect: &mut (dyn FnMut(String) + Send),
) -> OAuthResult<FlowResult> {
    let metadata_url = options
        .authorization_server_metadata_url
        .map(secure_endpoint)
        .transpose()?;
    // With a configured metadata URL, discovery is not cached (upstream
    // flow.ts:322).
    let cached = if metadata_url.is_some() {
        None
    } else {
        provider
            .store
            .load()
            .await?
            .and_then(|state| state.discovery)
    };
    let discovered = match cached.filter(|cached| !cached.authorization_server_url.is_empty()) {
        Some(cached) => {
            let authorization_server_url = reqwest::Url::parse(&cached.authorization_server_url)
                .map_err(|error| fatal(format!("Invalid authorization server URL: {error}")))?;
            let metadata = match cached.authorization_server_metadata {
                Some(metadata) => Some(metadata),
                None => {
                    discover_authorization_server_metadata(&authorization_server_url, false).await?
                }
            };
            OAuthServerInfo {
                authorization_server_url,
                authorization_server_metadata: metadata,
                resource_metadata: cached.resource_metadata,
            }
        }
        None => {
            discover_oauth_server_info(
                server_url,
                options.resource_metadata_url,
                metadata_url.as_ref().map(reqwest::Url::as_str),
            )
            .await?
        }
    };
    if metadata_url.is_none() {
        provider
            .save_discovery_state(OAuthDiscoveryState {
                authorization_server_url: discovered.authorization_server_url.to_string(),
                authorization_server_metadata: discovered.authorization_server_metadata.clone(),
                resource_metadata: discovered.resource_metadata.clone(),
                resource_metadata_url: options.resource_metadata_url.map(str::to_owned),
            })
            .await?;
    }
    let metadata = discovered.authorization_server_metadata.as_ref();
    let resource = select_resource(server_url, discovered.resource_metadata.as_ref())?;
    // `||`, not `??`: an empty scope falls through to the next source
    // (upstream flow.ts:352).
    let scope = options
        .scope
        .clone()
        .filter(|scope| !scope.is_empty())
        .or_else(|| {
            discovered
                .resource_metadata
                .as_ref()
                .and_then(|metadata| metadata.scopes_supported.as_ref())
                .filter(|scopes| !scopes.is_empty())
                .map(|scopes| scopes.join(" "))
        })
        .or_else(|| {
            provider
                .client_metadata
                .get("scope")
                .and_then(JsonValue::as_str)
                .map(str::to_owned)
        });
    let stored_client = provider.client_information().await?;
    let client_document = if stored_client.is_none() {
        provider.client_metadata_document(metadata)?
    } else {
        None
    };
    if let Some(document) = &client_document {
        let url = reqwest::Url::parse(&document.url)
            .map_err(|error| fatal(format!("Invalid OAuth client metadata URL: {error}")))?;
        if url.scheme() != "https" || url.path() == "/" {
            return Err(fatal("Invalid OAuth client metadata URL"));
        }
    }
    let mut client = stored_client.or_else(|| {
        client_document
            .as_ref()
            .map(|document| OAuthClientInformation {
                client_id: document.url.clone(),
                ..OAuthClientInformation::default()
            })
    });
    if client.is_none() {
        if options.authorization_code.is_some() {
            return Err(fatal(
                "OAuth client information is missing during code exchange",
            ));
        }
        let registered = register_client(
            &discovered.authorization_server_url,
            metadata,
            &provider.client_metadata,
            scope.as_deref(),
        )
        .await?;
        provider.save_client_information(&registered).await?;
        client = Some(registered);
    }
    let Some(client) = client else {
        return Err(fatal("OAuth client information is missing"));
    };
    // The document's redirect URI may differ from the provider's (upstream
    // flow.ts:374).
    let redirect_url = client_document
        .as_ref()
        .map(|document| document.redirect_url.clone())
        .unwrap_or_else(|| provider.redirect_url.clone());
    let token_options = TokenRequestOptions {
        metadata,
        client_information: &client,
        resource: resource.as_deref(),
    };
    if let Some(code) = options.authorization_code {
        // RFC 9207: never send a code from another authorization server to
        // this one. Without metadata there is no issuer to compare with, so
        // any `iss` is rejected instead of skipping the check (upstream
        // flow.ts:385-388).
        if let Some(mismatch) = authorization_issuer_mismatch(metadata, options.iss) {
            return Err(fatal(mismatch));
        }
        let tokens = exchange_authorization_code(
            &discovered.authorization_server_url,
            &token_options,
            code,
            &provider.code_verifier().await?,
            &redirect_url,
        )
        .await?;
        provider
            .save_tokens(with_scope(tokens, scope.as_deref()))
            .await?;
        return Ok(FlowResult::Authorized);
    }
    let existing = if options.skip_refresh {
        None
    } else {
        provider.tokens().await?
    };
    if let Some(existing) = existing.filter(|tokens| tokens.refresh_token.is_some()) {
        let refresh_token = existing.refresh_token.clone().unwrap_or_default();
        match refresh_authorization(
            &discovered.authorization_server_url,
            &token_options,
            &refresh_token,
        )
        .await
        {
            Ok(tokens) => {
                // A refresh without `scope` keeps the granted scope (RFC 6749
                // §6; upstream flow.ts:407).
                let tokens = with_scope(tokens, existing.scope.as_deref());
                provider.save_tokens(tokens).await?;
                return Ok(FlowResult::Authorized);
            }
            Err(error) => match error {
                McpOAuthError::InsecureEndpoint(_) => return Err(error),
                McpOAuthError::OAuth { ref code, .. } if code != "server_error" => {
                    return Err(error)
                }
                // Everything else falls through to the browser flow
                // (upstream flow.ts:410-413).
                _ => {}
            },
        }
    }
    let state = provider.state().await?;
    let authorization = start_authorization(
        &discovered.authorization_server_url,
        metadata,
        &client,
        &redirect_url,
        scope.as_deref(),
        Some(&state),
        resource.as_deref(),
    )?;
    provider
        .save_code_verifier(&authorization.code_verifier)
        .await?;
    on_redirect(authorization.authorization_url.to_string());
    Ok(FlowResult::Redirect)
}

/// Run the flow, retrying once after clearing credentials the server rejected
/// (upstream `authorizeMcp`).
async fn authorize_mcp(
    provider: &FlowProvider<'_>,
    server_url: &reqwest::Url,
    options: &FlowOptions<'_>,
    on_redirect: &mut (dyn FnMut(String) + Send),
) -> OAuthResult<FlowResult> {
    match run_flow(provider, server_url, options, on_redirect).await {
        Ok(result) => Ok(result),
        Err(McpOAuthError::OAuth { ref code, .. })
            if code == "invalid_client" || code == "unauthorized_client" =>
        {
            provider.invalidate_credentials(CredentialKind::All).await?;
            run_flow(provider, server_url, options, on_redirect).await
        }
        Err(McpOAuthError::OAuth { ref code, .. }) if code == "invalid_grant" => {
            provider
                .invalidate_credentials(CredentialKind::Tokens)
                .await?;
            run_flow(provider, server_url, options, on_redirect).await
        }
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// Configuration → settings and prompts
// ---------------------------------------------------------------------------

/// The `oauth` configuration of an HTTP MCP server, resolved
/// (upstream `McpOAuthSettings` / `oauthSettings()`).
#[derive(Debug, Clone, Default)]
pub struct McpOAuthSettings {
    /// Pre-registered client id; skips dynamic client registration.
    pub client_id: Option<String>,
    /// Already resolved (satisfies `${NAME}` / `!command` templates).
    pub client_secret: Option<String>,
    /// Fixed loopback callback port.
    pub callback_port: Option<u16>,
    /// Loopback redirect URI.
    pub callback_url: Option<String>,
    /// Scopes to request, separated by spaces.
    pub scope: Option<String>,
    /// `client_name` for dynamic client registration. Default: `pir`.
    pub client_name: Option<String>,
    /// Client registration strategy (dynamic registration or a Client ID
    /// Metadata Document).
    pub client_registration: Option<McpClientRegistration>,
    /// Authorization server metadata document to use instead of discovery.
    pub auth_server_metadata_url: Option<String>,
}

impl McpOAuthSettings {
    /// Resolve the `oauth` block of an HTTP server config (upstream
    /// `McpServerConnection.oauthSettings`, runtime.ts:238).
    pub fn from_config(config: &McpHttpServerConfig, server_name: &str) -> OAuthResult<Self> {
        let Some(oauth) = &config.oauth else {
            return Ok(Self::default());
        };
        let client_secret = match oauth.client_secret.as_deref() {
            None => None,
            Some(value) => Some(
                crate::ai::providers::config_value::resolve_config_value(value, None).ok_or_else(|| {
                    fatal(format!(
                        "MCP server \"{server_name}\" oauth.clientSecret \"{value}\" does not resolve (missing environment variable or failed command)"
                    ))
                })?,
            ),
        };
        Ok(Self {
            client_id: oauth.client_id.clone(),
            client_secret,
            callback_port: oauth.callback_port,
            callback_url: oauth.callback_url.clone(),
            scope: oauth.scope.clone(),
            client_name: oauth.client_name.clone(),
            client_registration: oauth.client_registration,
            auth_server_metadata_url: oauth.auth_server_metadata_url.clone(),
        })
    }

    /// Where the loopback callback server listens and the redirect URI it
    /// serves (upstream `callbackSettings`, oauth.ts:87).
    fn callback_settings(&self) -> OAuthResult<CallbackSettings> {
        let raw = self
            .callback_url
            .clone()
            .unwrap_or_else(|| format!("http://{DEFAULT_CALLBACK_HOST}{DEFAULT_CALLBACK_PATH}"));
        let url = reqwest::Url::parse(&raw)
            .map_err(|error| fatal(format!("invalid oauth.callbackUrl \"{raw}\": {error}")))?;
        let address = url
            .host_str()
            .unwrap_or(DEFAULT_CALLBACK_HOST)
            .trim_matches(|ch| ch == '[' || ch == ']')
            .to_string();
        let port = url.port().or(self.callback_port);
        let fixed_redirect_url = if url.port().is_some() {
            // A configured URI with a port is sent exactly as written, since
            // servers compare it as a string.
            Some(raw.clone())
        } else if let Some(port) = self.callback_port {
            let mut fixed = url.clone();
            fixed
                .set_port(Some(port))
                .map_err(|_| fatal(format!("invalid oauth.callbackUrl \"{raw}\"")))?;
            Some(fixed.to_string())
        } else {
            None
        };
        Ok(CallbackSettings {
            // `localhost` is served on 127.0.0.1; browsers fall back to it.
            host: if address == "localhost" {
                DEFAULT_CALLBACK_HOST.to_string()
            } else {
                address.clone()
            },
            redirect_host: address,
            port,
            path: url.path().to_string(),
            fixed_redirect_url,
        })
    }
}

/// Where the loopback callback server listens and the redirect URI it serves.
struct CallbackSettings {
    host: String,
    redirect_host: String,
    port: Option<u16>,
    path: String,
    fixed_redirect_url: Option<String>,
}

/// The interactive part of a sign-in (upstream `McpSignInPrompt`,
/// oauth.ts:376).
pub trait McpSignInPrompt: Send + Sync {
    /// Show the authorization URL to the user and open it in a browser.
    fn show_authorization_url(&self, url: &str);

    /// Ask for the redirect URL from the browser address bar, for when the
    /// browser cannot reach the loopback callback. `None` cancels the
    /// sign-in; an implementation without a terminal should never resolve
    /// (the callback or the timeout finishes the sign-in instead).
    fn prompt_for_redirect_url(&self) -> PromptFuture<'_>;
}

/// The boxed future returned by [`McpSignInPrompt::prompt_for_redirect_url`].
pub type PromptFuture<'a> = Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

// ---------------------------------------------------------------------------
// The credential store (upstream extensions/mcp/oauth.ts:114-224)
// ---------------------------------------------------------------------------

/// The stored states: `mcp-auth.json` is one object keyed by server.
type StoredStates = BTreeMap<String, McpOAuthState>;

/// `<agent-dir>/mcp-auth.json` (upstream `McpOAuthCredentialStore`).
#[derive(Debug, Clone)]
pub struct McpOAuthCredentialStore {
    auth_path: PathBuf,
}

impl McpOAuthCredentialStore {
    /// Open the store at `auth_path`.
    pub fn new(auth_path: PathBuf) -> Self {
        Self { auth_path }
    }

    /// The default store path `<agent-dir>/mcp-auth.json`.
    pub fn default_path() -> PathBuf {
        crate::coding_agent::context::agent_dir().join("mcp-auth.json")
    }

    /// Open the default store.
    pub fn default_store() -> Self {
        Self::new(Self::default_path())
    }

    /// The backing file path.
    pub fn path(&self) -> &Path {
        &self.auth_path
    }

    /// The per-server view of this store (upstream `forServer`).
    pub fn for_server(&self, name: &str, server_url: &str) -> McpOAuthServerStore {
        let legacy_key = reqwest::Url::parse(server_url)
            .map(|url| url.to_string())
            .unwrap_or_else(|_| server_url.to_string());
        McpOAuthServerStore {
            auth_path: self.auth_path.clone(),
            key: format!("{}|{}", mcp_namespace(name), legacy_key),
            legacy_key,
            server_url: reqwest::Url::parse(server_url)
                .map(|url| url.to_string())
                .unwrap_or_else(|_| server_url.to_string()),
        }
    }
}

/// One server's credentials inside an [`McpOAuthCredentialStore`].
#[derive(Debug, Clone)]
pub struct McpOAuthServerStore {
    auth_path: PathBuf,
    /// `<mcp namespace>|<url>`.
    key: String,
    /// `<url>`, written by older versions (upstream `legacyKey`).
    legacy_key: String,
    /// The normalized server URL this state belongs to.
    server_url: String,
}

impl McpOAuthServerStore {
    /// The store's file path (for diagnostics).
    pub fn path(&self) -> &Path {
        &self.auth_path
    }

    /// Load this server's state; another server's stored state is ignored so
    /// credentials never leak across servers (upstream `own`,
    /// provider.ts:164).
    pub async fn load(&self) -> OAuthResult<Option<McpOAuthState>> {
        let _lock = acquire_lock(
            &lock_path(&self.auth_path),
            STORE_LOCK_WAIT,
            STORE_LOCK_STALE,
        )
        .await?;
        let mut states = read_states(&self.auth_path);
        // The first server to load legacy state takes it over (upstream
        // oauth.ts:154-162).
        if !states.contains_key(&self.key) {
            if let Some(legacy) = states.remove(&self.legacy_key) {
                if legacy.server_url == self.server_url {
                    states.insert(self.key.clone(), legacy);
                    write_states(&self.auth_path, &states)?;
                }
            }
        }
        Ok(states
            .get(&self.key)
            .filter(|state| state.server_url == self.server_url)
            .cloned())
    }

    /// Replace this server's state.
    pub async fn save(&self, state: &McpOAuthState) -> OAuthResult<()> {
        let state = state.clone();
        self.update(move |stored| *stored = state).await
    }

    /// Serialized read-modify-write of this server's state.
    pub async fn update<F>(&self, f: F) -> OAuthResult<()>
    where
        F: FnOnce(&mut McpOAuthState) + Send,
    {
        let _lock = acquire_lock(
            &lock_path(&self.auth_path),
            STORE_LOCK_WAIT,
            STORE_LOCK_STALE,
        )
        .await?;
        let mut states = read_states(&self.auth_path);
        let mut state = states
            .remove(&self.key)
            .or_else(|| states.remove(&self.legacy_key))
            .filter(|state| state.server_url == self.server_url)
            .unwrap_or_default();
        state.server_url = self.server_url.clone();
        f(&mut state);
        states.insert(self.key.clone(), state);
        write_states(&self.auth_path, &states)
    }

    /// Remove this server's credentials; returns whether any were stored
    /// (upstream `remove`, oauth.ts:206).
    pub async fn remove(&self) -> OAuthResult<bool> {
        let _lock = acquire_lock(
            &lock_path(&self.auth_path),
            STORE_LOCK_WAIT,
            STORE_LOCK_STALE,
        )
        .await?;
        let mut states = read_states(&self.auth_path);
        let stored = if states.contains_key(&self.key) {
            Some(self.key.clone())
        } else if states.contains_key(&self.legacy_key) {
            Some(self.legacy_key.clone())
        } else {
            None
        };
        match stored {
            Some(key) => {
                states.remove(&key);
                write_states(&self.auth_path, &states)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// The stored tokens, if any (upstream `tokens`, oauth.ts:199).
    pub async fn tokens(&self) -> Option<OAuthTokens> {
        self.load()
            .await
            .ok()
            .flatten()
            .and_then(|state| state.tokens)
    }

    /// Run `f` while no other process refreshes this server's tokens
    /// (upstream `withRefreshLock`, oauth.ts:175).
    async fn with_refresh_lock<T>(
        &self,
        f: impl Future<Output = OAuthResult<T>>,
    ) -> OAuthResult<T> {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.key.as_bytes());
        let hash: String = digest
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let lock_dir = self
            .auth_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let lock = lock_dir.join(format!("mcp-auth-refresh-{hash}"));
        let _guard = acquire_lock(&lock, REFRESH_LOCK_WAIT, REFRESH_LOCK_STALE).await?;
        f.await
    }
}

fn lock_path(auth_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", auth_path.display()))
}

fn read_states(auth_path: &Path) -> StoredStates {
    let Ok(contents) = std::fs::read_to_string(auth_path) else {
        return StoredStates::new();
    };
    let trimmed = contents.strip_prefix('\u{feff}').unwrap_or(&contents);
    if trimmed.trim().is_empty() {
        return StoredStates::new();
    }
    serde_json::from_str::<StoredStates>(trimmed).unwrap_or_default()
}

/// Pretty JSON with a trailing newline (upstream `serializeStates`).
fn write_states(auth_path: &Path, states: &StoredStates) -> OAuthResult<()> {
    if let Some(parent) = auth_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| fatal(error.to_string()))?;
        }
    }
    let contents = serde_json::to_string_pretty(states)
        .map(|json| json + "\n")
        .map_err(|error| fatal(format!("failed to serialize mcp-auth.json: {error}")))?;
    write_private(auth_path, &contents).map_err(|error| fatal(error.to_string()))
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// RAII release for a cross-process lock file.
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Acquire a lock file with a blocking retry loop and stale-lock takeover
/// (the pattern of `crate::ai::auth::storage`, async flavor).
async fn acquire_lock(lock_path: &Path, wait: Duration, stale: Duration) -> OAuthResult<LockGuard> {
    if let Some(parent) = lock_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| fatal(error.to_string()))?;
        }
    }
    let deadline = Instant::now() + wait;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(_) => {
                return Ok(LockGuard {
                    path: lock_path.to_path_buf(),
                })
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let is_stale = std::fs::metadata(lock_path)
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .map(|modified| modified.elapsed().unwrap_or(Duration::ZERO) > stale)
                    .unwrap_or(false);
                if is_stale {
                    let _ = std::fs::remove_file(lock_path);
                    continue;
                }
                if Instant::now() >= deadline {
                    return Err(fatal(format!(
                        "Failed to acquire MCP OAuth lock after {}s: {}",
                        wait.as_secs(),
                        lock_path.display()
                    )));
                }
                tokio::time::sleep(LOCK_RETRY_DELAY).await;
            }
            Err(error) => return Err(fatal(error.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Connection helpers (upstream extensions/mcp/oauth.ts:300-374)
// ---------------------------------------------------------------------------

/// The server URL when the server authenticates with OAuth (upstream
/// `usesOAuth`, runtime.ts:83): HTTP servers without an `Authorization`
/// header and without an explicit `auth`.
pub fn oauth_server_url(config: &McpServerConfig) -> Option<&str> {
    let McpServerConfig::Http(http) = config else {
        return None;
    };
    if http.auth.is_some() {
        return None;
    }
    let has_authorization = http
        .headers
        .iter()
        .flatten()
        .any(|(name, _)| name.eq_ignore_ascii_case("authorization"));
    if has_authorization {
        return None;
    }
    Some(http.url.as_str())
}

/// Whether a failure means the server wants OAuth sign-in (upstream
/// `needsAuthorization`/`needsSignIn`): a 401, or a 403 whose challenge asks
/// for more scope (`insufficient_scope`).
pub fn is_authorization_required(error: &McpError) -> bool {
    match error {
        McpError::Http { status: 401, .. } => true,
        McpError::Http {
            status: 403, body, ..
        } => body.to_lowercase().contains("insufficient scope"),
        McpError::Transport(message) => {
            let lowered = message.to_lowercase();
            lowered.contains("authorization required") || lowered.contains("insufficient scope")
        }
        _ => false,
    }
}

/// Whether a failure is a bearer-token challenge (a 401, either as a typed
/// status or rmcp's `AuthRequired` transport error). A 403
/// `insufficient_scope` is an OAuth step-up case, not a stale token, so it is
/// not included: `auth: { provider }` servers must not start an OAuth flow.
pub fn is_unauthorized(error: &McpError) -> bool {
    match error {
        McpError::Http { status: 401, .. } => true,
        McpError::Transport(message) => message.to_lowercase().contains("authorization required"),
        _ => false,
    }
}

/// The message shown when a server needs a sign-in (upstream
/// `signInRequiredMessage`, runtime.ts:77-80).
///
/// Upstream reads the remedy off the server entry's config: a server whose
/// config delegates its token to a pi provider (`auth: { provider }`) is
/// signed in through `/login <provider>`, every other server through the
/// in-session `/mcp` command. Both are slash commands because the readers are
/// the model and the in-session user; the CLI's own hint (`pir mcp add`'s
/// `If it requires sign-in: pir mcp login <name>`) is the only place that keeps
/// naming the CLI, since `/mcp` does not exist outside a session.
///
/// A caller that holds the entry's config should use this function; the
/// name-only [`sign_in_required_message`] is the `/mcp` arm.
pub fn sign_in_required_message_for(config: &McpServerConfig, server: &str) -> String {
    match auth_provider(config) {
        Some(provider) => sign_in_required_message_provider(server, provider),
        None => sign_in_required_message(server),
    }
}

/// [`sign_in_required_message_for`] for the `auth: { provider }` arm: the
/// remedy is the provider's own login command (upstream runtime.ts:78-79).
pub fn sign_in_required_message_provider(server: &str, provider: &str) -> String {
    format!("MCP server \"{server}\" requires sign-in. Run /login {provider} to sign in.")
}

/// The pi provider a server's config names for its token (`auth.provider`),
/// or `None` when it has none (upstream `"url" in entry.config ?
/// entry.config.auth?.provider : undefined`, runtime.ts:78).
pub fn auth_provider(config: &McpServerConfig) -> Option<&str> {
    match config {
        McpServerConfig::Http(http) => http.auth.as_ref().map(|auth| auth.provider.as_str()),
        McpServerConfig::Stdio(_) => None,
    }
}

/// Whether a 401 on this server asks the user to sign in, rather than being a
/// plain authorization failure: an OAuth server (whose token the transport
/// refreshes) or one that delegates to a pi provider token (upstream
/// `needsSignIn`'s `authProvider !== undefined`).
pub fn has_sign_in_remedy(config: &McpServerConfig) -> bool {
    oauth_server_url(config).is_some() || auth_provider(config).is_some()
}

/// [`sign_in_required_message_for`] for a call site that has only the server
/// name. Every such site is the OAuth path — [`oauth_server_url`] returns
/// `None` when the config sets `auth` — so the `/mcp` wording is the right one
/// there.
pub fn sign_in_required_message(server: &str) -> String {
    format!("MCP server \"{server}\" requires sign-in. Run /mcp to sign in.")
}

/// The flow provider for one server and its settings.
fn flow_provider<'a>(
    store: &'a McpOAuthServerStore,
    settings: &McpOAuthSettings,
    server_url: &str,
    redirect_url: &str,
) -> OAuthResult<FlowProvider<'a>> {
    let client_metadata = serde_json::json!({
        "redirect_uris": [redirect_url],
        "token_endpoint_auth_method": if settings.client_secret.is_some() { "client_secret_post" } else { "none" },
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "client_name": settings.client_name.clone().unwrap_or_else(|| "pir".to_string()),
    });
    // A pre-registered client id skips registration (an empty one counts as
    // absent, like upstream's truthiness check).
    let configured_client = settings
        .client_id
        .as_deref()
        .filter(|client_id| !client_id.is_empty())
        .map(|client_id| OAuthClientInformation {
            client_id: client_id.to_string(),
            client_secret: settings.client_secret.clone(),
            ..OAuthClientInformation::default()
        });
    Ok(FlowProvider {
        server_url: server_url.to_string(),
        redirect_url: redirect_url.to_string(),
        client_metadata,
        client_metadata_document: settings.client_registration,
        configured_client,
        store,
    })
}

/// Resolve the access token to send with a connection (upstream
/// `createMcpAuthProvider().token`, oauth.ts:353): the stored token,
/// refreshed when it is about to expire and a refresh token exists. Failures
/// fall through to the old token — a 401 then decides what happens.
pub async fn resolve_access_token(
    store: &McpOAuthServerStore,
    settings: &McpOAuthSettings,
    server_url: &str,
) -> Option<String> {
    let state = store.load().await.ok().flatten()?;
    let tokens = state.tokens.as_ref()?;
    let expired = state
        .tokens_expire_at
        .map(|expires_at| expires_at - REFRESH_SKEW_MS <= now_ms())
        .unwrap_or(false);
    if !expired || tokens.refresh_token.is_none() {
        return Some(tokens.access_token.clone());
    }
    match refresh_access_token(store, settings, server_url, Some(&tokens.access_token)).await {
        Ok(token) => Some(token),
        Err(_) => Some(tokens.access_token.clone()),
    }
}

/// Refresh the stored tokens (upstream `createMcpAuthProvider().refresh`):
/// single-flight per server via the refresh lock, skipped when another
/// refresh or sign-in already replaced `stale_token`, and a fresh sign-in is
/// reported as [`McpOAuthError::AuthorizationRequired`].
pub async fn refresh_access_token(
    store: &McpOAuthServerStore,
    settings: &McpOAuthSettings,
    server_url: &str,
    stale_token: Option<&str>,
) -> OAuthResult<String> {
    let server_url = reqwest::Url::parse(server_url)
        .map_err(|error| fatal(format!("invalid MCP server URL \"{server_url}\": {error}")))?;
    let stale_token = stale_token.map(str::to_owned);
    store
        .with_refresh_lock(async {
            let state = store.load().await?;
            if let Some(current) = state.as_ref().and_then(|state| state.tokens.as_ref()) {
                // Tokens that changed meanwhile (another process refreshed
                // them, or the user signed in) are used without refreshing.
                if stale_token.is_some()
                    && Some(current.access_token.as_str()) != stale_token.as_deref()
                {
                    return Ok(current.access_token.clone());
                }
            }
            if state
                .as_ref()
                .and_then(|state| state.tokens.as_ref())
                .and_then(|tokens| tokens.refresh_token.as_ref())
                .is_none()
            {
                return Err(McpOAuthError::AuthorizationRequired);
            }
            let callback = settings.callback_settings()?;
            let redirect_url = refresh_redirect_url(store, &callback).await;
            let provider = flow_provider(store, settings, server_url.as_str(), &redirect_url)?;
            match authorize_mcp(&provider, &server_url, &FlowOptions::default(), &mut |_| {})
                .await?
            {
                FlowResult::Authorized => store
                    .load()
                    .await?
                    .and_then(|state| state.tokens)
                    .map(|tokens| tokens.access_token)
                    .filter(|token| !token.is_empty())
                    .ok_or_else(|| fatal("OAuth refresh stored no access token")),
                FlowResult::Redirect => Err(McpOAuthError::AuthorizationRequired),
            }
        })
        .await
}

/// The redirect URI a refresh uses when none is stored (upstream oauth.ts:330):
/// the configured URI, else the first registered one, else the default.
async fn refresh_redirect_url(store: &McpOAuthServerStore, callback: &CallbackSettings) -> String {
    if let Some(fixed) = &callback.fixed_redirect_url {
        return fixed.clone();
    }
    let registered = store
        .load()
        .await
        .ok()
        .flatten()
        .and_then(|state| state.client_information)
        .and_then(|information| information.redirect_uris().into_iter().next());
    registered.unwrap_or_else(|| FALLBACK_REDIRECT_URL.to_string())
}

// ---------------------------------------------------------------------------
// Sign-in (upstream `signInMcpServer`, oauth.ts:473)
// ---------------------------------------------------------------------------

/// A validated authorization response.
#[derive(Debug, Clone)]
struct AuthorizationResponse {
    code: String,
    iss: Option<String>,
}

/// A pasted redirect URL as an authorization response (upstream
/// `responseFromRedirectUrl`, oauth.ts:396).
fn response_from_redirect_url(
    input: &str,
    state: &str,
    redirect_url: &reqwest::Url,
) -> OAuthResult<AuthorizationResponse> {
    let url = reqwest::Url::parse(input.trim())
        .map_err(|_| fatal("Expected the full redirect URL from the browser address bar"))?;
    // A server-specific redirect URI tells authorization servers apart, so it
    // must match exactly.
    if url.origin() != redirect_url.origin() || url.path() != redirect_url.path() {
        return Err(fatal(
            "The redirect URL does not match this sign-in's redirect URI",
        ));
    }
    let parameters: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let parameter = |name: &str| {
        parameters
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    if let Some(error) = parameter("error") {
        return Err(fatal(
            parameter("error_description").unwrap_or(error).to_string(),
        ));
    }
    if parameter("state") != Some(state) {
        return Err(fatal("The redirect URL belongs to a different sign-in"));
    }
    let Some(code) = parameter("code").filter(|code| !code.is_empty()) else {
        return Err(fatal(
            "The redirect URL does not contain an authorization code",
        ));
    };
    Ok(AuthorizationResponse {
        code: code.to_string(),
        iss: parameter("iss").map(str::to_owned),
    })
}

/// Listen on `port`, or on a free port when it is taken and not required
/// (upstream `listenForCallback`, oauth.ts:444).
async fn listen_for_callback(
    settings: &CallbackSettings,
    path: &str,
    state: &str,
    port: Option<u16>,
    required: bool,
) -> OAuthResult<OAuthCallbackServer> {
    let start = |port: u16| {
        OAuthCallbackServer::start(
            "MCP server",
            &settings.host,
            port,
            path,
            Some(&settings.redirect_host),
            Some(state),
        )
    };
    match start(port.unwrap_or(0)).await {
        Ok(server) => Ok(server),
        Err(error) => {
            if required || port.is_none() {
                return Err(fatal(format!(
                    "could not start the OAuth callback server: {error}"
                )));
            }
            start(0).await.map_err(|error| {
                fatal(format!(
                    "could not start the OAuth callback server: {error}"
                ))
            })
        }
    }
}

/// Wait for the browser callback or a pasted redirect URL, whichever comes
/// first, bounded by the sign-in deadline (upstream
/// `waitForAuthorizationResponse`, oauth.ts:419).
async fn wait_for_authorization_response(
    callback: &OAuthCallbackServer,
    state: &str,
    redirect_url: &reqwest::Url,
    prompt: &dyn McpSignInPrompt,
    deadline: Instant,
) -> OAuthResult<AuthorizationResponse> {
    let wait = async {
        tokio::select! {
            waited = callback.wait(None) => match waited {
                Ok(CallbackWait::Completed(pairs)) => {
                    let code = query_param(&pairs, "code")
                        .filter(|code| !code.is_empty())
                        .ok_or_else(|| fatal("OAuth callback did not include an authorization code"))?;
                    Ok(AuthorizationResponse {
                        code: code.to_string(),
                        iss: query_param(&pairs, "iss").map(str::to_owned),
                    })
                }
                Ok(CallbackWait::Cancelled) => Err(McpOAuthError::Cancelled),
                Err(error) => Err(fatal(error.to_string())),
            },
            pasted = prompt.prompt_for_redirect_url() => match pasted {
                Some(input) if !input.trim().is_empty() => {
                    response_from_redirect_url(&input, state, redirect_url)
                }
                _ => Err(McpOAuthError::Cancelled),
            }
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    match tokio::time::timeout(remaining, wait).await {
        Ok(result) => result,
        Err(_) => Err(McpOAuthError::Cancelled),
    }
}

/// Sign in to an MCP server (upstream `signInMcpServer`, oauth.ts:473): reuse
/// the stored refresh token when possible, otherwise run the browser
/// authorization code flow against a loopback callback. Tokens are saved to
/// `store`.
pub async fn sign_in(
    store: &McpOAuthServerStore,
    settings: &McpOAuthSettings,
    server_url: &str,
    challenge: Option<&OAuthChallenge>,
    prompt: &dyn McpSignInPrompt,
    timeout: Duration,
) -> OAuthResult<()> {
    let server_url = reqwest::Url::parse(server_url)
        .map_err(|error| fatal(format!("invalid MCP server URL \"{server_url}\": {error}")))?;
    let deadline = Instant::now() + timeout;
    let stored = store.load().await?;
    let step_up =
        challenge.and_then(|challenge| challenge.error.as_deref()) == Some("insufficient_scope");
    let callback = settings.callback_settings()?;
    let cimd = settings.client_registration == Some(McpClientRegistration::Cimd);
    // Reuse the port of the registered redirect URI so the registered client
    // stays valid (upstream oauth.ts:487).
    let registered_redirect = stored
        .as_ref()
        .and_then(|state| state.client_information.as_ref())
        .and_then(|information| information.redirect_uris().into_iter().next());
    let preferred_port = callback.port.or_else(|| {
        registered_redirect
            .as_deref()
            .and_then(|url| reqwest::Url::parse(url).ok())
            .and_then(|url| url.port())
    });
    // A server-specific Client ID Metadata Document needs a server-specific
    // callback path on the loopback server (upstream `extraPaths`, oauth.ts:494);
    // with `iss` support the shared document and the default path are used.
    let path = if cimd {
        let iss_supported = discover_oauth_server_info(
            &server_url,
            challenge.and_then(|challenge| challenge.resource_metadata_url.as_deref()),
            None,
        )
        .await
        .ok()
        .and_then(|info| info.authorization_server_metadata)
        .and_then(|metadata| metadata.authorization_response_iss_parameter_supported)
        .unwrap_or(false);
        if iss_supported {
            callback.path.clone()
        } else {
            format!(
                "{DEFAULT_CALLBACK_PATH}/{}",
                callback_id(server_url.as_str())
            )
        }
    } else {
        callback.path.clone()
    };
    // Every sign-in gets a fresh `state` parameter (upstream oauth.ts:503),
    // enforced by the loopback callback server.
    let state = random_state();
    let state_for_store = state.clone();
    store
        .update(|stored| stored.oauth_state = Some(state_for_store))
        .await?;
    let callback_server = listen_for_callback(
        &callback,
        &path,
        &state,
        preferred_port,
        callback.port.is_some(),
    )
    .await?;
    let redirect_url = callback
        .fixed_redirect_url
        .clone()
        .unwrap_or_else(|| callback_server.redirect_uri().to_string());
    let flow = async {
        if let Some(existing) = &stored {
            let mut next = existing.clone();
            // Keep the fresh `state` persisted above (every sign-in gets a
            // fresh one; upstream oauth.ts:503).
            next.oauth_state = Some(state.clone());
            // A registered client cannot use another redirect URI, and its
            // tokens belong to it. A Client ID Metadata Document is not
            // stored, so with one, a stored client was registered before and
            // is replaced (upstream oauth.ts:506-513).
            let keep_client = settings
                .client_id
                .as_deref()
                .is_some_and(|client_id| !client_id.is_empty())
                || if cimd {
                    existing.client_information.is_none()
                } else {
                    existing
                        .client_information
                        .as_ref()
                        .is_some_and(|information| {
                            information
                                .redirect_uris()
                                .iter()
                                .any(|url| url == &redirect_url)
                        })
                };
            if !keep_client {
                next.client_information = None;
                next.tokens = None;
                next.tokens_expire_at = None;
            }
            store.save(&next).await?;
        }
        let provider = flow_provider(store, settings, server_url.as_str(), &redirect_url)?;
        // A server asking for more scope gets it on top of the configured
        // scope and, since the challenge may list only the missing scopes, on
        // top of the scope granted so far (upstream oauth.ts:527-532).
        let challenged = if step_up {
            step_up_scope(
                stored
                    .as_ref()
                    .and_then(|state| state.tokens.as_ref())
                    .and_then(|tokens| tokens.scope.as_deref()),
                challenge.and_then(|challenge| challenge.scope.as_deref()),
            )
        } else {
            challenge.and_then(|challenge| challenge.scope.clone())
        };
        let flow_options = FlowOptions {
            scope: merge_scopes(settings.scope.as_deref(), challenged.as_deref()),
            resource_metadata_url: challenge
                .and_then(|challenge| challenge.resource_metadata_url.as_deref()),
            // A refresh keeps the granted scope; a server asking for more
            // needs the browser flow.
            skip_refresh: step_up,
            ..FlowOptions::default()
        };
        let mut authorization_url: Option<String> = None;
        let mut record_redirect = |url: String| authorization_url = Some(url);
        if authorize_mcp(&provider, &server_url, &flow_options, &mut record_redirect).await?
            == FlowResult::Authorized
        {
            return Ok(());
        }
        let Some(authorization_url) = authorization_url else {
            return Err(fatal("OAuth flow did not produce an authorization URL"));
        };
        // The flow picks the redirect URI, which may be specific to the MCP
        // server (upstream oauth.ts:541).
        let authorization_redirect_url = reqwest::Url::parse(&authorization_url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "redirect_uri")
                    .map(|(_, value)| value.into_owned())
            })
            .and_then(|value| reqwest::Url::parse(&value).ok())
            .unwrap_or_else(|| server_url.clone());
        prompt.show_authorization_url(&authorization_url);
        let response = wait_for_authorization_response(
            &callback_server,
            &state,
            &authorization_redirect_url,
            prompt,
            deadline,
        )
        .await?;
        let code_options = FlowOptions {
            authorization_code: Some(&response.code),
            iss: response.iss.as_deref(),
            ..flow_options
        };
        match authorize_mcp(&provider, &server_url, &code_options, &mut |_| {}).await? {
            FlowResult::Authorized => Ok(()),
            FlowResult::Redirect => Err(fatal("OAuth flow did not complete the sign-in")),
        }
    };
    // The deadline bounds the WHOLE sign-in, not just the browser wait
    // (upstream `signInMcpServer`'s `AbortSignal.timeout`): dropping the
    // flow future cancels an in-flight authorization-server request, so a
    // timed-out sign-in sends nothing afterwards.
    let result: OAuthResult<()> =
        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), flow).await {
            Ok(result) => result,
            Err(_) => Err(McpOAuthError::Cancelled),
        };
    // Answer the browser page with the real sign-in outcome (upstream sends
    // the page only after the code exchange settles): success renders 200, a
    // failure renders 502 carrying the error.
    callback_server.confirm_page(result.as_ref().err().map(|error| error.to_string()));
    callback_server.close();
    result
}

/// Scopes of both lists, each once (upstream `mergeScopes`, oauth.ts:109).
fn merge_scopes(first: Option<&str>, second: Option<&str>) -> Option<String> {
    let mut scopes: Vec<&str> = Vec::new();
    for scope in [first, second].into_iter().flatten() {
        for item in scope.split_whitespace().filter(|item| !item.is_empty()) {
            if !scopes.contains(&item) {
                scopes.push(item);
            }
        }
    }
    if scopes.is_empty() {
        None
    } else {
        Some(scopes.join(" "))
    }
}
