//! Vertex AI service-account authentication (OAuth2 JWT-bearer → Bearer token).
//!
//! Rust port of pi's Application-Default-Credentials path for `google-vertex`.
//! pi hands the credential file to `@google/genai` (2.21.0) →
//! `google-auth-library` (10.6.2), which locate the key, sign an RS256 JWT and
//! exchange it for an OAuth2 access token; this module reimplements that flow
//! with the `rsa` crate. The libraries are not vendored in the pi checkout, so
//! the cited line numbers are those of the npm builds pinned by pi's
//! package-lock.json (`google-auth-library@10.6.2`, `@google/genai@2.21.0`).
//!
//! Credential-file resolution order — pi passes
//! `googleAuthOptions = { keyFilename }` from `GOOGLE_APPLICATION_CREDENTIALS`
//! (packages/ai/src/api/google-vertex.ts:424-427); without it GoogleAuth falls
//! back to its own ADC chain (googleauth.js:246-283):
//!
//! 1. `GOOGLE_APPLICATION_CREDENTIALS` (and its lowercase alias
//!    `google_application_credentials`, googleauth.js:314-317).
//! 2. The well-known gcloud ADC file — `%APPDATA%\gcloud\
//!    application_default_credentials.json` on Windows, otherwise
//!    `$HOME/.config/gcloud/application_default_credentials.json`
//!    (googleauth.js:331-360). pi's provider status check reads the literal
//!    `~/.config/...` path (providers/google-vertex.ts:6,76), but the loader is
//!    the library's, so `%APPDATA%` is the Windows location here.
//! 3. The GCE metadata server (googleauth.js:279-283) — deliberately not
//!    implemented; the typed error covers it.
//!
//! Only `type: "service_account"` key files are supported. Upstream dispatches
//! `authorized_user` / `external_account` / `impersonated_service_account` to
//! other google-auth-library flows (googleauth.js:476-505) that pir does not
//! implement — those file types keep the explicit typed gap.
//!
//! JWT + token exchange (google-auth-library 10.6.2):
//! - header `{"alg":"RS256"}` (gtoken/jwsSign.js:20,46-52);
//! - claims `iss=client_email`, `scope=https://www.googleapis.com/auth/cloud-platform`
//!   (the scope @google/genai requires, dist/node/index.cjs:23790-23791),
//!   `aud=https://oauth2.googleapis.com/token` — a constant; the key file's
//!   `token_uri` is ignored by v10 (gtoken/jwsSign.js:22,28-40) —, `iat=now`,
//!   `exp=iat+3600`, and no `sub` (pi never sets a subject);
//! - exchange: `POST https://oauth2.googleapis.com/token` with
//!   `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer` +
//!   `assertion=<jwt>` as `application/x-www-form-urlencoded`
//!   (gtoken/getToken.js:19-40);
//! - error bodies are reported as `${error}: ${error_description}`
//!   (gtoken/getToken.js:55-64);
//! - token expiry is `now + expires_in * 1000`, `expires_in ?? 0`
//!   (gtoken/tokenHandler.js:81-88), and a cached token is refreshed once it is
//!   within 5 minutes of expiring — `DEFAULT_EAGER_REFRESH_THRESHOLD_MILLIS`
//!   (auth/authclient.js:29-31,48) enforced by `isTokenExpiring`
//!   (auth/oauth2client.js:812-819).
//!
//! Deviation from upstream: upstream builds a fresh GoogleGenAI client per
//! stream call (google-vertex.ts:101-103), so its token cache lives for a
//! single request. pir caches the bearer process-wide keyed by the credential
//! identity (client_email + SHA-256 of the private key, so a rotated key never
//! reuses a stale token), which keeps the same refresh policy without a token
//! exchange on every request.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use reqwest::Method;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::Sha256 as RsaSha256;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::ai::api::provider_env_value;
use crate::ai::error::{AiError, AiResult};
use crate::ai::transport::ProviderRequestClient;
use crate::ai::types::{AbortSignal, ProviderEnv};

/// Scope @google/genai requires for Vertex (`REQUIRED_VERTEX_AI_SCOPE`,
/// @google/genai@2.21.0 dist/node/index.cjs:23791); pi passes only
/// `{ keyFilename }`, so this default scope is what GoogleAuth applies
/// (index.cjs:23847-23860, auth/googleauth.js:501-504).
const VERTEX_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// `GoogleToken` endpoint constant — v10 no longer honors the key file's
/// `token_uri` (google-auth-library 10.6.2 gtoken/jwsSign.js:22,33 and
/// gtoken/getToken.js:19).
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// JWT-bearer grant type (gtoken/getToken.js:21).
const JWT_BEARER_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// JWT lifetime: `exp = iat + 3600` (gtoken/jwsSign.js:34).
const JWT_LIFETIME_SECONDS: u64 = 3600;

/// Refresh a cached token within 5 minutes of expiry
/// (google-auth-library auth/authclient.js:29-31,48).
const EAGER_REFRESH_THRESHOLD: Duration = Duration::from_secs(5 * 60);

/// A resolved service-account bearer, plus the credential's quota project.
pub(super) struct BearerToken {
    pub access_token: String,
    /// Key-file `quota_project_id` (jwtclient.js:241) or, on the well-known
    /// ADC-file path, a `GOOGLE_CLOUD_QUOTA_PROJECT` override
    /// (googleauth.js:283-291). Sent as `x-goog-user-project`
    /// (auth/authclient.js:145-152); never overrides a caller header.
    pub quota_project_id: Option<String>,
}

/// The service-account key fields this port consumes.
struct ServiceAccountKey {
    client_email: String,
    private_key_pem: String,
    quota_project_id: Option<String>,
}

/// `gcloud auth application-default login` credentials
/// (`type: "authorized_user"`): a refresh token exchanged at Google's OAuth2
/// token endpoint, the other ADC type `google-auth-library` supports.
struct AuthorizedUserKey {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    quota_project_id: Option<String>,
}

/// The ADC credential types pir can mint bearer tokens for.
enum AdcCredential {
    ServiceAccount(ServiceAccountKey),
    AuthorizedUser(AuthorizedUserKey),
}

impl AdcCredential {
    /// `(cache key, key-file quota project)` for the credential.
    fn cache_identity(&self) -> (String, Option<String>) {
        match self {
            AdcCredential::ServiceAccount(key) => (
                token_cache_key(&key.client_email, &key.private_key_pem),
                key.quota_project_id.clone(),
            ),
            AdcCredential::AuthorizedUser(key) => (
                authorized_user_cache_key(&key.client_id, &key.refresh_token),
                key.quota_project_id.clone(),
            ),
        }
    }
}

/// Cached bearer for one credential identity.
struct CachedBearer {
    access_token: String,
    expires_at: Instant,
}

/// Process-wide token cache (see the module deviation note).
static BEARER_CACHE: OnceLock<Mutex<HashMap<String, CachedBearer>>> = OnceLock::new();

/// Resolve an access token for the ADC/service-account path: locate the key
/// file, sign the RS256 assertion and exchange it (or reuse the cached bearer
/// while it is more than the eager-refresh threshold from expiry).
pub(super) async fn bearer_token(
    client: &ProviderRequestClient,
    env: Option<&ProviderEnv>,
    signal: Option<&AbortSignal>,
) -> AiResult<BearerToken> {
    let (path, from_env) = credentials_file_path(env).ok_or_else(no_adc_error)?;
    let contents = read_key_file(&path, from_env)?;
    let credential = parse_adc_credential(&contents, &path)?;
    let (cache_key, key_quota_project) = credential.cache_identity();

    // `GOOGLE_CLOUD_QUOTA_PROJECT` overrides the key-file value only on the
    // well-known-file ADC path (googleauth.js:283-291); pi's keyFilename path
    // skips `#prepareAndCacheClient` entirely (#determineClient,
    // googleauth.js:746-752).
    let quota_project_id = if from_env {
        key_quota_project
    } else {
        provider_env_value("GOOGLE_CLOUD_QUOTA_PROJECT", env)
            .filter(|value| !value.is_empty())
            .or(key_quota_project)
    };

    if let Some(access_token) = cached_token(&cache_key) {
        return Ok(BearerToken {
            access_token,
            quota_project_id,
        });
    }

    let (access_token, expires_in_seconds) = match &credential {
        AdcCredential::ServiceAccount(key) => {
            let assertion = build_assertion(&key.client_email, &key.private_key_pem)?;
            exchange_assertion(client, &assertion, signal).await?
        }
        AdcCredential::AuthorizedUser(key) => exchange_refresh_token(client, key, signal).await?,
    };
    // tokenHandler.js:86-87: `tokenExpiresAt = Date.now() + (expires_in ?? 0) * 1000`.
    // `expires_in` is server-controlled; a huge value must not panic
    // `Instant + Duration` (which would abort the request worker).
    let expires_at = Instant::now()
        .checked_add(Duration::from_secs(expires_in_seconds))
        .unwrap_or_else(Instant::now);
    store_token(&cache_key, &access_token, expires_at);
    Ok(BearerToken {
        access_token,
        quota_project_id,
    })
}

/// Resolve the credential file per the ADC chain: env var first
/// (googleauth.js:314-317 — the `keyFilename` pi forwards), then the
/// well-known gcloud file (googleauth.js:331-360). Returns
/// `(path, came_from_env)`. The metadata-server step is intentionally absent.
fn credentials_file_path(env: Option<&ProviderEnv>) -> Option<(PathBuf, bool)> {
    for name in [
        "GOOGLE_APPLICATION_CREDENTIALS",
        "google_application_credentials",
    ] {
        if let Some(path) = provider_env_value(name, env).filter(|value| !value.is_empty()) {
            return Some((PathBuf::from(path), true));
        }
    }
    let path = well_known_adc_path()?;
    path.is_file().then_some((path, false))
}

/// The gcloud well-known ADC file location (googleauth.js:331-360):
/// `%APPDATA%\gcloud\...` on Windows, `$HOME/.config/gcloud/...` elsewhere.
fn well_known_adc_path() -> Option<PathBuf> {
    let root = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
    }?;
    Some(
        root.join("gcloud")
            .join("application_default_credentials.json"),
    )
}

/// The typed "no ADC configured" error (google-auth-library's NO_ADC_FOUND
/// text, googleauth.js:281-283, plus the paths pir checked).
fn no_adc_error() -> AiError {
    let well_known = well_known_adc_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "the gcloud well-known ADC path".to_string());
    AiError::Authentication(format!(
        "Could not load the default credentials for Google Vertex AI. Browse to \
         https://cloud.google.com/docs/authentication/getting-started for more information. \
         Set GOOGLE_APPLICATION_CREDENTIALS to a service-account JSON key file (also set \
         GOOGLE_CLOUD_PROJECT and GOOGLE_CLOUD_LOCATION), or run `gcloud auth application-default \
         login` and place the credentials at {well_known}; the GCE metadata-server fallback is not \
         implemented in this build of pir. Vertex AI express mode with a GOOGLE_CLOUD_API_KEY \
         remains available."
    ))
}

/// Read the key file. Error text follows googleauth.js:320-327 for the
/// env-var path; the file path is included for either source.
fn read_key_file(path: &Path, from_env: bool) -> AiResult<String> {
    std::fs::read_to_string(path).map_err(|error| {
        if from_env {
            AiError::Authentication(format!(
                "Unable to read the credential file specified by the GOOGLE_APPLICATION_CREDENTIALS \
                 environment variable: {error}"
            ))
        } else {
            AiError::Authentication(format!(
                "Unable to read the Google application default credentials file {}: {error}",
                path.display()
            ))
        }
    })
}

/// Parse the JSON key file: `type` dispatch (googleauth.js:476-505),
/// `client_email` / `private_key` requirements (jwtclient.js:230-235) and
/// `quota_project_id` (jwtclient.js:241). `authorized_user` credentials (the
/// `gcloud auth application-default login` output) carry a refresh token
/// instead, exchanged at the same token endpoint. Key material never enters
/// errors.
fn parse_adc_credential(contents: &str, path: &Path) -> AiResult<AdcCredential> {
    let value: Value = serde_json::from_str(contents).map_err(|error| {
        // getCredentials.js:50-57 — "Invalid JSON key file: <parse error>".
        AiError::Authentication(format!("Invalid JSON key file {}: {error}", path.display()))
    })?;
    let credential_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("service_account");
    let quota_project_id = value
        .get("quota_project_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    match credential_type {
        "service_account" => {
            let client_email = value
                .get("client_email")
                .and_then(Value::as_str)
                .filter(|email| !email.is_empty())
                .ok_or_else(|| {
                    AiError::Authentication(format!(
                        "The incoming JSON object does not contain a client_email field ({})",
                        path.display()
                    ))
                })?;
            let private_key_pem = value
                .get("private_key")
                .and_then(Value::as_str)
                .filter(|key| !key.is_empty())
                .ok_or_else(|| {
                    AiError::Authentication(format!(
                        "The incoming JSON object does not contain a private_key field ({})",
                        path.display()
                    ))
                })?;
            Ok(AdcCredential::ServiceAccount(ServiceAccountKey {
                client_email: client_email.to_string(),
                private_key_pem: private_key_pem.to_string(),
                quota_project_id,
            }))
        }
        "authorized_user" => {
            let required = |field: &str| -> AiResult<String> {
                value
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        AiError::Authentication(format!(
                            "The authorized_user credentials file {} does not contain a {field} field",
                            path.display()
                        ))
                    })
            };
            Ok(AdcCredential::AuthorizedUser(AuthorizedUserKey {
                client_id: required("client_id")?,
                client_secret: required("client_secret")?,
                refresh_token: required("refresh_token")?,
                quota_project_id,
            }))
        }
        // external_account / impersonated_service_account still need other
        // google-auth-library flows (googleauth.js:476-505); keep the typed gap.
        other => Err(AiError::Authentication(format!(
            "Google Vertex AI credential type '{other}' ({}) is not supported in this \
             build of pir; supported ADC types are 'service_account' and 'authorized_user'. \
             Use one of those files, or configure Vertex AI express mode with a \
             GOOGLE_CLOUD_API_KEY instead.",
            path.display()
        ))),
    }
}

/// Build the signed JWT assertion: `base64url(header).base64url(claims)` +
/// RS256 signature, claims per gtoken/jwsSign.js:28-40 (see module docs).
fn build_assertion(client_email: &str, private_key_pem: &str) -> AiResult<String> {
    let now_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            AiError::Authentication(format!("system clock is before the Unix epoch: {error}"))
        })?
        .as_secs();
    let header = json!({ "alg": "RS256" });
    let claims = json!({
        "iss": client_email,
        "scope": VERTEX_SCOPE,
        "aud": GOOGLE_TOKEN_URL,
        "exp": now_seconds + JWT_LIFETIME_SECONDS,
        "iat": now_seconds,
    });
    let header_segment = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);
    let claims_segment = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
    let signing_input = format!("{header_segment}.{claims_segment}");

    let key = parse_rsa_private_key(private_key_pem)?;
    let signing_key = SigningKey::<RsaSha256>::new(key);
    let signature = signing_key
        .try_sign(signing_input.as_bytes())
        .map_err(|error| {
            AiError::Authentication(format!(
                "failed to sign the Vertex AI service-account JWT: {error}"
            ))
        })?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

/// Strict PEM parsing of the RSA private key; Node's `createPrivateKey`
/// accepts PKCS#8 and PKCS#1, so both are tried and anything else fails typed.
/// The pem-rfc7468 decoder rejects malformed labels/content, and no error path
/// includes key material.
fn parse_rsa_private_key(pem: &str) -> AiResult<RsaPrivateKey> {
    match RsaPrivateKey::from_pkcs8_pem(pem) {
        Ok(key) => Ok(key),
        Err(pkcs8_error) => RsaPrivateKey::from_pkcs1_pem(pem).map_err(|pkcs1_error| {
            AiError::Authentication(format!(
                "service-account private_key is not a valid PKCS#8 or PKCS#1 RSA PEM key \
                 ({pkcs8_error}; {pkcs1_error})"
            ))
        }),
    }
}

/// POST the JWT-bearer grant (gtoken/getToken.js:27-40) and return
/// `(access_token, expires_in)`; token-handler retry/backoff comes from the
/// shared transport, gaxios retries POST for the same statuses
/// (getToken.js:36-38).
async fn exchange_assertion(
    client: &ProviderRequestClient,
    assertion: &str,
    signal: Option<&AbortSignal>,
) -> AiResult<(String, u64)> {
    let body = format!(
        "grant_type={}&assertion={}",
        form_urlencode(JWT_BEARER_GRANT_TYPE),
        form_urlencode(assertion)
    );
    post_token_form(client, &body, signal).await
}

/// Exchange an `authorized_user` refresh token for an access token (the
/// `gcloud auth application-default login` flow): `grant_type=refresh_token`
/// with the client id/secret from the ADC file.
async fn exchange_refresh_token(
    client: &ProviderRequestClient,
    key: &AuthorizedUserKey,
    signal: Option<&AbortSignal>,
) -> AiResult<(String, u64)> {
    let body = format!(
        "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
        form_urlencode(&key.client_id),
        form_urlencode(&key.client_secret),
        form_urlencode(&key.refresh_token),
    );
    post_token_form(client, &body, signal).await
}

/// POST one form body to the OAuth2 token endpoint and parse
/// `{access_token, expires_in}`.
async fn post_token_form(
    client: &ProviderRequestClient,
    body: &str,
    signal: Option<&AbortSignal>,
) -> AiResult<(String, u64)> {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert(
        "content-type".to_string(),
        "application/x-www-form-urlencoded".to_string(),
    );
    let mut bytes: Vec<u8> = Vec::new();
    let mut on_chunk = |chunk: &[u8]| -> AiResult<()> {
        bytes.extend_from_slice(chunk);
        Ok(())
    };
    client
        .request_streaming(
            Method::POST,
            GOOGLE_TOKEN_URL,
            &headers,
            body.as_bytes().to_vec(),
            signal,
            // Token-exchange HTTP (not an AI provider request): no hooks.
            &crate::ai::types::ProviderStreamHooks::default(),
            &mut on_chunk,
        )
        .await
        .map_err(token_request_error)?;

    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        AiError::Authentication(format!(
            "Google OAuth token endpoint returned an invalid JSON response: {error}"
        ))
    })?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            AiError::Authentication(
                "Google OAuth token endpoint response did not include an access_token".to_string(),
            )
        })?;
    // tokenHandler.js:87 — `expires_in ?? 0` (a missing value means "refresh
    // next request", not "never expires").
    let expires_in_seconds = value.get("expires_in").and_then(Value::as_u64).unwrap_or(0);
    Ok((access_token.to_string(), expires_in_seconds))
}

/// Map a non-2xx token-endpoint response to the upstream error shape:
/// `${error}: ${error_description}` (gtoken/getToken.js:55-64). The transport
/// embeds the status and (truncated) body as `HTTP <status>: <body>`.
fn token_request_error(error: AiError) -> AiError {
    match error {
        AiError::NonRetryable(message) => AiError::Authentication(format_token_error(&message)),
        other => other,
    }
}

fn format_token_error(message: &str) -> String {
    let Some((_, body)) = message.split_once(": ") else {
        return message.to_string();
    };
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return message.to_string();
    };
    match (
        value.get("error").and_then(Value::as_str),
        value.get("error_description").and_then(Value::as_str),
    ) {
        (Some(error), Some(description)) => format!("{error}: {description}"),
        (Some(error), None) => error.to_string(),
        _ => message.to_string(),
    }
}

/// WHATWG `URLSearchParams` application/x-www-form-urlencoded encoding (the
/// form JS posts, getToken.js:31-34): bytes outside `A-Za-z0-9*-._` are
/// percent-encoded, spaces become `+`.
fn form_urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                encoded.push(byte as char)
            }
            b' ' => encoded.push('+'),
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

/// Cache key for one credential identity: the RSA key is part of the identity
/// so a rotated key never reuses a token minted for the old key. Only the
/// email and a SHA-256 digest leave this function — never key material.
fn token_cache_key(client_email: &str, private_key_pem: &str) -> String {
    let digest = Sha256::digest(private_key_pem.as_bytes());
    let fingerprint: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{client_email}:{fingerprint}")
}

/// Cache key for an `authorized_user` credential: the client id plus a
/// SHA-256 digest of the refresh token (only the digest leaves this function).
fn authorized_user_cache_key(client_id: &str, refresh_token: &str) -> String {
    let digest = Sha256::digest(refresh_token.as_bytes());
    let fingerprint: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("authorized_user:{client_id}:{fingerprint}")
}

/// A cached bearer more than the eager-refresh threshold from expiry
/// (oauth2client.js:812-819: refresh when `expiry_date <= now + threshold`).
fn cached_token(cache_key: &str) -> Option<String> {
    let cache = BEARER_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let guard = cache.lock().ok()?;
    let entry = guard.get(cache_key)?;
    if entry.expires_at <= Instant::now() + EAGER_REFRESH_THRESHOLD {
        return None;
    }
    Some(entry.access_token.clone())
}

fn store_token(cache_key: &str, access_token: &str, expires_at: Instant) {
    let cache = BEARER_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.insert(
            cache_key.to_string(),
            CachedBearer {
                access_token: access_token.to_string(),
                expires_at,
            },
        );
    }
}
