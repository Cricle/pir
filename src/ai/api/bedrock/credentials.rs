//! AWS credential chain for the Bedrock adapter (task G5 / P1-5).
//!
//! Closes the "Bedrock env-only credentials" deviation from upstream: beyond
//! static env credentials and the bearer token, this resolves the shared
//! profile files (`~/.aws/credentials` + `~/.aws/config`, `AWS_PROFILE`),
//! ECS container credentials (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` /
//! `AWS_CONTAINER_CREDENTIALS_FULL_URI`), and web identity token files
//! (`AWS_WEB_IDENTITY_TOKEN_FILE` + `AWS_ROLE_ARN` via the anonymous
//! `sts:AssumeRoleWithWebIdentity` call) — the pieces upstream gets from the
//! AWS SDK default provider chain.
//!
//! Resolution order: an explicitly configured `AWS_PROFILE` wins over
//! ambient env keys (upstream #6957), then static env, web identity, ECS
//! container credentials, the `default` shared profile, and finally the EC2
//! instance-metadata (IMDSv2) link. The shared-profile links fully support
//! the upstream profile features pir can honor locally: `credential_process`
//! (spawned without a shell, JSON output), `role_arn` + `source_profile`
//! assume-role (SigV4-signed `sts:AssumeRole`), and IAM Identity Center (SSO)
//! profiles, which exchange the cached `aws sso login` access token for role
//! credentials through the portal's `GetRoleCredentials` API. The SDK default
//! chain this mirrors is
//! env → profile → web identity → ECS container → EC2 IMDS (pir keeps its
//! pre-existing explicit-`AWS_PROFILE`-first / `default`-profile-fallback
//! placement and puts IMDS last, matching the SDK's relative
//! profile-before-IMDS and IMDS-terminating order). When nothing resolves,
//! the chain is exhausted into the typed `NoCredentials` error. Every
//! configured-but-failing step is fail-closed with a typed
//! [`AiError::Authentication`]; nothing panics and every network call is
//! bounded by a timeout — except the IMDS link, which fails fast and
//! silently (it is a chain step, not a terminal error).

use std::collections::BTreeMap;
use std::time::Duration;

use regex::Regex;

use crate::ai::api::provider_env_value;
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::ProviderEnv;

/// Bound on the ECS credentials endpoint fetch (the SDK uses a short
/// container-metadata budget; the endpoint is link-local).
pub const ECS_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on the STS `AssumeRoleWithWebIdentity` call.
pub const STS_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound on one `credential_process` invocation (the SDK's 1-minute default,
/// tightened to 30s here).
pub const CREDENTIAL_PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
/// IMDSv2 connect bound (non-EC2 hosts must fail fast).
pub const IMDS_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Per-request IMDSv2 bound.
pub const IMDS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Whole-flow IMDSv2 budget: token + role lookup + credentials fetch must
/// finish inside this or the link is skipped silently.
pub const IMDS_TOTAL_BUDGET: Duration = Duration::from_secs(3);
/// IMDSv2 token TTL requested via `x-aws-ec2-metadata-token-ttl-seconds`.
pub const IMDS_TOKEN_TTL_SECONDS: &str = "21600";
/// Default EC2 instance-metadata endpoint.
pub const IMDS_ENDPOINT: &str = "http://169.254.169.254";
/// Maximum assume-role nesting followed through `source_profile` chains.
const MAX_ASSUME_ROLE_DEPTH: usize = 5;
/// Cap on any single STS/ECS response body (both payloads are tiny XML/JSON
/// documents; the cap bounds memory against a hostile endpoint).
const MAX_RESPONSE_BODY_BYTES: u64 = 4 * 1024 * 1024;

/// Resolved AWS credentials plus where they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// Human-readable source (`environment`, `web-identity`, `ecs-container`,
    /// `profile:<name>`).
    pub source: String,
}

/// A resolved credential plus a region discovered from the profile files
/// (`None` unless a shared profile supplied one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBedrockAuth {
    pub credentials: AwsCredentials,
    pub region: Option<String>,
}

/// Which link of the chain a given environment selects (pure planning step,
/// exercised by tests/ without touching process state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourcePlan {
    /// An explicit `AWS_PROFILE` resolves first (upstream #6957: a configured
    /// profile wins over ambient env keys).
    Profile { name: String },
    /// Both `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` are set.
    StaticEnv,
    /// `AWS_WEB_IDENTITY_TOKEN_FILE` (+ `AWS_ROLE_ARN`).
    WebIdentity,
    /// ECS container credentials endpoint (relative or full URI).
    Ecs { url: String },
    /// Only one of the static env key pair is set — fail closed (SDK
    /// semantics: the pair must be specified together).
    StaticEnvIncomplete,
    /// `AWS_CONTAINER_CREDENTIALS_FULL_URI` without an http(s) scheme.
    InvalidEcsUri { value: String },
    /// `AWS_CONTAINER_CREDENTIALS_FULL_URI` points at a host outside the
    /// loopback/link-local range the SDK permits.
    InvalidEcsHost { value: String },
    /// Nothing configured anywhere and no shared profile files exist — the
    /// terminal typed error (the `default` shared profile is attempted first
    /// when the files are present).
    NoCredentials,
}

// ---------------------------------------------------------------------------
// Chain planning + resolution
// ---------------------------------------------------------------------------

/// Plan which credential source applies, in chain order. Pure: all inputs are
/// passed in so tests can cover the order without process env mutations.
pub fn plan_source(
    profile: Option<&str>,
    access_key: Option<&str>,
    secret_key: Option<&str>,
    web_identity_token_file: Option<&str>,
    ecs_relative_uri: Option<&str>,
    ecs_full_uri: Option<&str>,
) -> SourcePlan {
    if let Some(name) = profile.filter(|name| !name.is_empty()) {
        return SourcePlan::Profile {
            name: name.to_string(),
        };
    }
    match (
        access_key.filter(|v| !v.is_empty()),
        secret_key.filter(|v| !v.is_empty()),
    ) {
        (Some(_), Some(_)) => return SourcePlan::StaticEnv,
        (Some(_), None) | (None, Some(_)) => return SourcePlan::StaticEnvIncomplete,
        (None, None) => {}
    }
    if web_identity_token_file.filter(|v| !v.is_empty()).is_some() {
        return SourcePlan::WebIdentity;
    }
    if let Some(relative) = ecs_relative_uri.filter(|v| !v.is_empty()) {
        return SourcePlan::Ecs {
            url: format!("http://169.254.170.2{}", relative),
        };
    }
    if let Some(full) = ecs_full_uri.filter(|v| !v.is_empty()) {
        if full.starts_with("http://") || full.starts_with("https://") {
            if ecs_full_uri_host_allowed(full) {
                return SourcePlan::Ecs {
                    url: full.to_string(),
                };
            }
            return SourcePlan::InvalidEcsHost {
                value: full.to_string(),
            };
        }
        return SourcePlan::InvalidEcsUri {
            value: full.to_string(),
        };
    }
    SourcePlan::NoCredentials
}

/// Whether a `AWS_CONTAINER_CREDENTIALS_FULL_URI` points at a host the AWS
/// SDK permits: loopback (`localhost`, `127.0.0.0/8`, `[::1]`) or link-local
/// (`169.254.0.0/16`, which covers the ECS metadata addresses 169.254.170.2
/// and 169.254.170.23). Anything else is rejected.
pub fn ecs_full_uri_host_allowed(full_uri: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(full_uri) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // Ipv6 hosts serialize with brackets; strip them before parsing.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(address)) => {
            let octets = address.octets();
            octets[0] == 127 || (octets[0] == 169 && octets[1] == 254)
        }
        Ok(std::net::IpAddr::V6(address)) => address.is_loopback(),
        Err(_) => false,
    }
}

/// Resolve the full credential chain for one Bedrock request.
pub async fn resolve_credentials(env: Option<&ProviderEnv>) -> AiResult<ResolvedBedrockAuth> {
    let plan = plan_source(
        provider_env_value("AWS_PROFILE", env).as_deref(),
        provider_env_value("AWS_ACCESS_KEY_ID", env).as_deref(),
        provider_env_value("AWS_SECRET_ACCESS_KEY", env).as_deref(),
        provider_env_value("AWS_WEB_IDENTITY_TOKEN_FILE", env).as_deref(),
        provider_env_value("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", env).as_deref(),
        provider_env_value("AWS_CONTAINER_CREDENTIALS_FULL_URI", env).as_deref(),
    );
    match plan {
        SourcePlan::Profile { name } => load_profile_async(&name).await,
        SourcePlan::StaticEnv => Ok(ResolvedBedrockAuth {
            credentials: AwsCredentials {
                access_key_id: provider_env_value("AWS_ACCESS_KEY_ID", env).unwrap_or_default(),
                secret_access_key: provider_env_value("AWS_SECRET_ACCESS_KEY", env)
                    .unwrap_or_default(),
                session_token: provider_env_value("AWS_SESSION_TOKEN", env),
                source: "environment".to_string(),
            },
            region: None,
        }),
        SourcePlan::WebIdentity => web_identity_credentials(env).await,
        SourcePlan::Ecs { url } => Ok(ResolvedBedrockAuth {
            credentials: fetch_ecs_credentials(&url).await?,
            region: None,
        }),
        SourcePlan::StaticEnvIncomplete => Err(AiError::Authentication(
            "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must both be set when either is present"
                .to_string(),
        )),
        SourcePlan::InvalidEcsUri { value } => Err(AiError::Authentication(format!(
            "AWS_CONTAINER_CREDENTIALS_FULL_URI must use http(s): {value}"
        ))),
        SourcePlan::InvalidEcsHost { value } => Err(AiError::Authentication(format!(
            "AWS_CONTAINER_CREDENTIALS_FULL_URI host must be loopback or link-local \
             (SDK container-credential restriction): {value}"
        ))),
        SourcePlan::NoCredentials => {
            // SDK order: the shared-profile provider precedes the EC2 IMDS
            // provider, and IMDS terminates the chain. A present-but-failing
            // `default` profile stays fail-closed (never silently skipped).
            if shared_files_present() {
                return load_profile_async("default").await;
            }
            if let Some(credentials) = imds_credentials().await {
                return Ok(ResolvedBedrockAuth {
                    credentials,
                    region: None,
                });
            }
            Err(AiError::Authentication(
                "No AWS credentials for Bedrock: set AWS_BEARER_TOKEN_BEDROCK, \
                 AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, AWS_PROFILE, \
                 AWS_WEB_IDENTITY_TOKEN_FILE, run inside an ECS task role, or \
                 attach an EC2 instance role"
                    .to_string(),
            ))
        }
    }
}

/// The terminal chain link (P1-2): the `default` shared profile, used only
/// when no other source is configured. `None` when no shared file exists at
/// all — the chain is genuinely exhausted and the caller reports
/// `NoCredentials`. A present-but-failing `default` profile keeps the
/// existing fail-closed semantics (missing keys, unsupported `role_arn`,
/// ... surface as typed errors, never silent `None`). Public so the
/// independent test binary can exercise the fallback against fixtures.
///
/// The live chain resolves the `default` profile through
/// [`load_profile_async`] instead (which additionally supports
/// `credential_process`, `role_arn` + `source_profile`, and SSO); this
/// synchronous variant keeps the static-only behavior for direct test use.
pub fn default_profile_fallback() -> Option<AiResult<ResolvedBedrockAuth>> {
    if !shared_files_present() {
        return None;
    }
    Some(load_profile("default"))
}

/// Whether at least one of the shared credential/config files exists (the
/// chain only attempts the profile links when they do).
fn shared_files_present() -> bool {
    let (credentials_path, config_path) = shared_file_paths();
    credentials_path.is_some_and(|path| path.is_file())
        || config_path.is_some_and(|path| path.is_file())
}

// ---------------------------------------------------------------------------
// Shared profile files (~/.aws/credentials + ~/.aws/config)
// ---------------------------------------------------------------------------

/// Parsed shared AWS files: sections keyed by their literal header
/// (`credentials` uses `[name]`, `config` uses `[profile name]` /
/// `[default]`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AwsProfileFiles {
    pub credentials: BTreeMap<String, BTreeMap<String, String>>,
    pub config: BTreeMap<String, BTreeMap<String, String>>,
}

/// Parse an AWS-style INI document: `[section]` headers, `key = value` pairs
/// (keys lower-cased), `#`/`;` comments, surrounding quotes stripped, and
/// trailing-backslash continuation lines joined with a space. Malformed lines
/// are skipped (a shared file may contain profiles pir never reads).
pub fn parse_ini(content: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    // Continuation buffer: the pending (key, partial value) awaiting more
    // lines because the value ended with a backslash.
    let mut pending: Option<(String, String)> = None;

    for raw in content.lines() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim();
        let is_continuation = matches!(&pending, Some((_, value)) if value.ends_with('\\'));
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            // Blank lines and comments terminate a pending continuation.
            if !is_continuation {
                continue;
            }
            flush_pending(&mut sections, &current, &mut pending);
            continue;
        }
        if is_continuation {
            // Any non-blank, non-comment line continues the pending value
            // (with or without an `=` in it). The pending value still carries
            // its trailing backslash; it is popped here and at flush time.
            let Some((_, pending_value)) = pending.as_mut() else {
                continue;
            };
            pending_value.pop(); // drop the previous trailing backslash
            pending_value.push(' ');
            pending_value.push_str(trimmed);
            if !trimmed.ends_with('\\') {
                flush_pending(&mut sections, &current, &mut pending);
            }
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            flush_pending(&mut sections, &current, &mut pending);
            let name = unquote(trimmed[1..trimmed.len() - 1].trim());
            if !name.is_empty() {
                current = Some(name);
            }
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = unquote(key.trim()).to_ascii_lowercase();
            if key.is_empty() {
                continue;
            }
            let value = unquote(value.trim());
            if value.ends_with('\\') {
                // Keep the trailing backslash: it marks the pending
                // continuation and is popped when the next line appends (or
                // at flush time for a dangling marker).
                pending = Some((key, value));
            } else {
                sections
                    .entry(current.clone().unwrap_or_default())
                    .or_default()
                    .insert(key, value);
            }
        }
    }
    flush_pending(&mut sections, &current, &mut pending);
    sections
}

/// Commit a pending (possibly continued) key/value into its section.
fn flush_pending(
    sections: &mut BTreeMap<String, BTreeMap<String, String>>,
    current: &Option<String>,
    pending: &mut Option<(String, String)>,
) {
    if let Some((key, value)) = pending.take() {
        let value = value.trim_end_matches('\\').trim().to_string();
        sections
            .entry(current.clone().unwrap_or_default())
            .or_default()
            .insert(key, value);
    }
}

/// Strip one pair of matching surrounding quotes.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        return value[1..value.len() - 1].to_string();
    }
    value.to_string()
}

/// Parse both shared files (`None` = file absent).
pub fn parse_profile_files(credentials: Option<&str>, config: Option<&str>) -> AwsProfileFiles {
    AwsProfileFiles {
        credentials: credentials.map(parse_ini).unwrap_or_default(),
        config: config.map(parse_ini).unwrap_or_default(),
    }
}

/// A profile's merged entries: the `config` section supplies the base
/// (looked up as `[profile name]` except for `default`), the credentials-file
/// section overlays it (the SDK gives the credentials file precedence for the
/// same key).
pub fn profile_entries(files: &AwsProfileFiles, name: &str) -> Option<BTreeMap<String, String>> {
    let config_section = if name == "default" {
        files.config.get(name)
    } else {
        files
            .config
            .get(&format!("profile {name}"))
            .or_else(|| files.config.get(name))
    };
    let credentials_section = files.credentials.get(name);
    if config_section.is_none() && credentials_section.is_none() {
        return None;
    }
    let mut merged = config_section.cloned().unwrap_or_default();
    if let Some(section) = credentials_section {
        for (key, value) in section {
            merged.insert(key.clone(), value.clone());
        }
    }
    Some(merged)
}

/// Extract static credentials (and the profile region) from parsed shared
/// files. `origin` names the file pair for error messages. Fail-closed: a
/// missing or incomplete profile is a typed error.
///
/// This is the synchronous, static-only subset: `role_arn`,
/// `credential_process`, and `sso_session` profiles are rejected here even
/// where the live chain (via [`resolve_profile`]) supports all three — kept
/// for the independent test binaries, which assert the sync subset's
/// fail-closed behavior.
pub fn profile_credentials_from(
    files: &AwsProfileFiles,
    name: &str,
    origin: &str,
) -> AiResult<ResolvedBedrockAuth> {
    let entries = profile_entries(files, name).ok_or_else(|| {
        AiError::Authentication(format!("AWS profile '{name}' not found in {origin}"))
    })?;
    for feature in ["role_arn", "credential_process", "sso_session"] {
        if entries.contains_key(feature) {
            return Err(AiError::Authentication(format!(
                "AWS profile '{name}' uses '{feature}', which pir's Bedrock adapter \
                 does not support"
            )));
        }
    }
    static_profile_credentials(&entries, name, origin)
}

/// Build static-key credentials (and the profile region) from merged profile
/// entries. Shared by the synchronous subset above and the async resolver.
fn static_profile_credentials(
    entries: &BTreeMap<String, String>,
    name: &str,
    origin: &str,
) -> AiResult<ResolvedBedrockAuth> {
    let access_key_id = entries.get("aws_access_key_id").map(String::as_str);
    let secret_access_key = entries.get("aws_secret_access_key").map(String::as_str);
    match (access_key_id, secret_access_key) {
        (Some(access), Some(secret)) if !access.is_empty() && !secret.is_empty() => {
            Ok(ResolvedBedrockAuth {
                credentials: AwsCredentials {
                    access_key_id: access.to_string(),
                    secret_access_key: secret.to_string(),
                    session_token: entries
                        .get("aws_session_token")
                        .map(String::as_str)
                        .filter(|token| !token.is_empty())
                        .map(str::to_string),
                    source: format!("profile:{name}"),
                },
                region: entries
                    .get("region")
                    .map(String::as_str)
                    .filter(|region| !region.is_empty())
                    .map(str::to_string),
            })
        }
        (access, secret) => {
            let missing = match (access, secret) {
                (None, _) => "aws_access_key_id",
                (Some(_), None) => "aws_secret_access_key",
                _ => "non-empty credentials",
            };
            Err(AiError::Authentication(format!(
                "AWS profile '{name}' is missing {missing} in {origin}"
            )))
        }
    }
}

/// Resolve a shared profile from the filesystem (`AWS_SHARED_CREDENTIALS_FILE`
/// / `AWS_CONFIG_FILE` or `~/.aws/credentials` / `~/.aws/config`).
pub fn load_profile(name: &str) -> AiResult<ResolvedBedrockAuth> {
    let (credentials_path, config_path) = shared_file_paths();
    let credentials = read_optional(credentials_path.as_ref())?;
    let config = read_optional(config_path.as_ref())?;
    let origin = shared_origin(credentials_path.as_ref(), config_path.as_ref());
    profile_credentials_from(
        &parse_profile_files(credentials.as_deref(), config.as_deref()),
        name,
        &origin,
    )
}

/// The shared file paths: env overrides first, then the home directory.
fn shared_file_paths() -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok();
    let credentials = std::env::var("AWS_SHARED_CREDENTIALS_FILE")
        .ok()
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            home.as_ref().map(|home| {
                std::path::PathBuf::from(home)
                    .join(".aws")
                    .join("credentials")
            })
        });
    let config = std::env::var("AWS_CONFIG_FILE")
        .ok()
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            home.as_ref()
                .map(|home| std::path::PathBuf::from(home).join(".aws").join("config"))
        });
    (credentials, config)
}

/// The human-readable origin of the shared files for error messages (a
/// missing path falls back to the default home location's name).
fn shared_origin(
    credentials_path: Option<&std::path::PathBuf>,
    config_path: Option<&std::path::PathBuf>,
) -> String {
    format!(
        "{} / {}",
        credentials_path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "~/.aws/credentials".to_string()),
        config_path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "~/.aws/config".to_string()),
    )
}

/// Read a file when it exists; a missing file yields `None` (the chain moves
/// on), while an unreadable present file is a typed error.
fn read_optional(path: Option<&std::path::PathBuf>) -> AiResult<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.is_file() {
        return Ok(None);
    }
    std::fs::read_to_string(path).map(Some).map_err(|error| {
        AiError::Authentication(format!(
            "failed to read AWS config file {}: {error}",
            path.display()
        ))
    })
}

// ---------------------------------------------------------------------------
// Async profile resolution (credential_process + assume-role + SSO)
// ---------------------------------------------------------------------------

/// Resolve a shared profile from the filesystem, with full support for the
/// profile features the live chain honors: `credential_process` (spawned
/// without a shell), `role_arn` + `source_profile` (SigV4-signed
/// `sts:AssumeRole` against the source profile's base credentials), and
/// IAM Identity Center (SSO) profiles (the cached access token from
/// `aws sso login` exchanged for role credentials). Fail-closed throughout:
/// a configured-but-failing feature never silently degrades.
pub async fn load_profile_async(name: &str) -> AiResult<ResolvedBedrockAuth> {
    let (credentials_path, config_path) = shared_file_paths();
    let credentials = read_optional(credentials_path.as_ref())?;
    let config = read_optional(config_path.as_ref())?;
    let origin = shared_origin(credentials_path.as_ref(), config_path.as_ref());
    resolve_profile(
        &parse_profile_files(credentials.as_deref(), config.as_deref()),
        name,
        &origin,
    )
    .await
}

/// Resolve one profile from already-parsed shared files. Public so the
/// independent test binary can drive it against fixtures; everything past
/// parsing is async because `credential_process`, assume-role, and SSO
/// profiles may execute a process, sign an STS call, or read the SSO token
/// cache.
pub async fn resolve_profile(
    files: &AwsProfileFiles,
    name: &str,
    origin: &str,
) -> AiResult<ResolvedBedrockAuth> {
    resolve_profile_inner(files, name, origin, 0, &[]).await
}

/// Boxed so `source_profile` chains can recurse (async fns cannot call
/// themselves directly). `ancestors` carries the profiles already visited on
/// this path for cycle detection.
fn resolve_profile_inner<'a>(
    files: &'a AwsProfileFiles,
    name: &'a str,
    origin: &'a str,
    depth: usize,
    ancestors: &'a [String],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<ResolvedBedrockAuth>> + Send + 'a>>
{
    Box::pin(async move {
        if depth > MAX_ASSUME_ROLE_DEPTH {
            return Err(AiError::Authentication(format!(
                "AWS profile assume-role chain is nested deeper than \
                 {MAX_ASSUME_ROLE_DEPTH} levels (at '{name}')"
            )));
        }
        let entries = profile_entries(files, name).ok_or_else(|| {
            AiError::Authentication(format!("AWS profile '{name}' not found in {origin}"))
        })?;
        if entries.contains_key("sso_session") || entries.contains_key("sso_start_url") {
            if entries.contains_key("sso_account_id") || entries.contains_key("sso_role_name") {
                return sso_profile_credentials(files, &entries).await;
            }
            return Err(AiError::Authentication(format!(
                "AWS profile '{name}' uses SSO without 'sso_account_id'/'sso_role_name' \
                 (the sso_session/SSO form requires both), which pir's Bedrock adapter \
                 cannot resolve"
            )));
        }
        // Fail closed: a configured-but-empty value must reach
        // `run_credential_process`, whose splitter raises the typed
        // "could not be split" error — silently skipping here would fall
        // through to static credentials with no mention of the feature.
        if let Some(command) = entries
            .get("credential_process")
            .map(String::as_str)
            .map(str::trim)
        {
            let credentials = run_credential_process(command).await?;
            return Ok(ResolvedBedrockAuth {
                credentials: AwsCredentials {
                    source: "credential-process".to_string(),
                    ..credentials
                },
                region: profile_region(&entries),
            });
        }
        if let Some(role_arn) = entries
            .get("role_arn")
            .map(String::as_str)
            .map(str::trim)
            .filter(|arn| !arn.is_empty())
        {
            let source_name = entries
                .get("source_profile")
                .map(String::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    AiError::Authentication(format!(
                        "AWS profile '{name}' uses 'role_arn' without 'source_profile', \
                         which pir's Bedrock adapter does not support"
                    ))
                })?;
            if ancestors.iter().any(|visited| visited == source_name) {
                return Err(AiError::Authentication(format!(
                    "AWS profile assume-role chain contains a cycle at '{source_name}'"
                )));
            }
            let mut next: Vec<String> = ancestors.to_vec();
            next.push(name.to_string());
            let source =
                resolve_profile_inner(files, source_name, origin, depth + 1, &next).await?;
            let region = profile_region(&entries)
                .or(source.region.clone())
                .or_else(|| region_from_env("AWS_REGION"))
                .or_else(|| region_from_env("AWS_DEFAULT_REGION"))
                .unwrap_or_else(|| "us-east-1".to_string());
            if !is_valid_sts_region(&region) {
                return Err(AiError::Authentication(format!(
                    "AWS region '{region}' is not a valid AWS region (expected [a-z0-9-]+); \
                     refusing to build the STS endpoint for '{name}'"
                )));
            }
            let session_name = std::env::var("AWS_ROLE_SESSION_NAME")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "pir".to_string());
            let credentials = sts_assume_role(
                &sts_endpoint(&region),
                &region,
                role_arn,
                &session_name,
                &source.credentials,
            )
            .await?;
            return Ok(ResolvedBedrockAuth {
                credentials: AwsCredentials {
                    source: "assume-role".to_string(),
                    ..credentials
                },
                region: profile_region(&entries),
            });
        }
        static_profile_credentials(&entries, name, origin)
    })
}

/// A profile's `region` entry, when present and non-empty.
fn profile_region(entries: &BTreeMap<String, String>) -> Option<String> {
    entries
        .get("region")
        .map(String::as_str)
        .map(str::trim)
        .filter(|region| !region.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// IAM Identity Center (SSO) profiles
// ---------------------------------------------------------------------------

/// Bound on one SSO portal `GetRoleCredentials` call.
pub const SSO_TIMEOUT: Duration = Duration::from_secs(15);

/// A cached SSO access token (`~/.aws/sso/cache/*.json`, the AWS SDK
/// `SSOToken` cache shape).
#[derive(Debug, Clone, serde::Deserialize)]
struct SsoCachedToken {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expiresAt")]
    expires_at: Option<String>,
    #[serde(rename = "startUrl")]
    start_url: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(rename = "sessionName", default)]
    session_name: Option<String>,
}

/// Parse an ISO-8601 UTC timestamp as Unix seconds. The SSO token cache uses
/// `2026-10-25T12:00:00UTC`; the portal credentials payload uses epoch
/// milliseconds instead. Returns `None` for an unparseable value.
///
/// The year is range-checked (`1970..=9999`) before the civil-date arithmetic:
/// the local cache file is untrusted input, and an out-of-range year must fail
/// closed instead of producing an arbitrary epoch value (the extreme values
/// would also overflow the day/second arithmetic on some targets).
pub fn parse_iso8601_utc_seconds(value: &str) -> Option<i64> {
    let value = value.trim();
    let value = value
        .strip_suffix("UTC")
        .or_else(|| value.strip_suffix('Z'))
        .unwrap_or(value);
    let (date, time) = value.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts
        .next()
        .and_then(|second| second.split('.').next())
        .and_then(|second| second.parse().ok())
        .unwrap_or(0);
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let year_adjusted = year - if month <= 2 { 1 } else { 0 };
    let era = if year_adjusted >= 0 {
        year_adjusted
    } else {
        year_adjusted - 399
    } / 400;
    let year_of_era = year_adjusted - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// The SSO token cache directory (`AWS_SSO_CACHE_DIR` or `~/.aws/sso/cache`).
fn sso_cache_dir() -> Option<std::path::PathBuf> {
    if let Ok(value) = std::env::var("AWS_SSO_CACHE_DIR") {
        if !value.trim().is_empty() {
            return Some(std::path::PathBuf::from(value.trim()));
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(
        std::path::PathBuf::from(home)
            .join(".aws")
            .join("sso")
            .join("cache"),
    )
}

/// Find a cached, unexpired access token for the SSO session. Matching is by
/// `startUrl` (plus `region`/`sessionName` when the cache entry carries them),
/// which avoids reproducing the SDK's SHA-1 cache-key hashing.
fn find_cached_sso_token(
    start_url: &str,
    region: &str,
    session_name: Option<&str>,
) -> Option<SsoCachedToken> {
    let directory = sso_cache_dir()?;
    let entries = std::fs::read_dir(directory).ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(token) = serde_json::from_str::<SsoCachedToken>(&body) else {
            continue;
        };
        if token.access_token.trim().is_empty() {
            continue;
        }
        if token.start_url.as_deref() != Some(start_url) {
            continue;
        }
        if token
            .region
            .as_deref()
            .is_some_and(|cached_region| cached_region != region)
        {
            continue;
        }
        if let (Some(expected), Some(cached)) = (session_name, token.session_name.as_deref()) {
            if cached != expected {
                continue;
            }
        }
        // Fail closed: a cache entry without a parseable `expiresAt` (missing,
        // malformed, or outside the accepted year range) is not trusted. The
        // previous `is_none_or` treated an unparseable timestamp as unexpired,
        // which would accept an attacker-controlled cache file indefinitely.
        let unexpired = token
            .expires_at
            .as_deref()
            .and_then(parse_iso8601_utc_seconds)
            .is_some_and(|expires| expires > now + 30);
        if !unexpired {
            continue;
        }
        candidates.push(token);
    }
    candidates.into_iter().next()
}

/// Resolve an SSO-backed profile: find the session's cached token, then
/// exchange it for role credentials through the IAM Identity Center portal
/// (`GetRoleCredentials`, what the AWS SDK's SSO credential provider does).
async fn sso_profile_credentials(
    files: &AwsProfileFiles,
    entries: &BTreeMap<String, String>,
) -> AiResult<ResolvedBedrockAuth> {
    let required = |key: &str| -> AiResult<String> {
        entries
            .get(key)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                AiError::Authentication(format!(
                    "AWS SSO profile is missing '{key}' (required to resolve role credentials)"
                ))
            })
    };
    let session_name = entries
        .get("sso_session")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    // `sso_session` names a `[sso-session <name>]` config section carrying the
    // start URL and region; legacy profiles declare them inline.
    let (start_url, sso_region) = match session_name {
        Some(session) => {
            let section = files
                .config
                .get(&format!("sso-session {session}"))
                .ok_or_else(|| {
                    AiError::Authentication(format!(
                        "AWS profile references sso-session '{session}', which is not \
                         defined in the config file"
                    ))
                })?;
            let pick = |key: &str| {
                section
                    .get(key)
                    .map(String::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            let start_url = pick("sso_start_url").ok_or_else(|| {
                AiError::Authentication(format!(
                    "AWS sso-session '{session}' is missing 'sso_start_url'"
                ))
            })?;
            let region = pick("sso_region").ok_or_else(|| {
                AiError::Authentication(format!(
                    "AWS sso-session '{session}' is missing 'sso_region'"
                ))
            })?;
            (start_url, region)
        }
        None => (required("sso_start_url")?, required("sso_region")?),
    };
    let account_id = required("sso_account_id")?;
    let role_name = required("sso_role_name")?;

    let token = find_cached_sso_token(&start_url, &sso_region, session_name).ok_or_else(|| {
        AiError::Authentication(format!(
            "no cached, unexpired SSO access token for {start_url} in {} (run \
             'aws sso login' first)",
            sso_cache_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "~/.aws/sso/cache".to_string())
        ))
    })?;

    let client = credentials_client(SSO_TIMEOUT, "SSO credentials")?;
    let url = format!(
        "https://portal.sso.{sso_region}.amazonaws.com/federation/credentials\
         ?roleName={}&accountId={}",
        form_urlencode_component(&role_name),
        form_urlencode_component(&account_id),
    );
    let response = client
        .get(&url)
        .header("x-amz-sso_bearer_token", &token.access_token)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|error| {
            AiError::Http(format!("SSO GetRoleCredentials request failed: {error}"))
        })?;
    let status = response.status().as_u16();
    let body = read_bounded_body(response, "SSO GetRoleCredentials").await?;
    if !(200..300).contains(&status) {
        return Err(AiError::Authentication(format!(
            "SSO GetRoleCredentials failed (HTTP {status}): {}",
            truncate_body(&body)
        )));
    }
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| AiError::Http(format!("unreadable SSO credentials body: {error}")))?;
    let role = value
        .get("roleCredentials")
        .filter(|value| value.is_object())
        .ok_or_else(|| {
            AiError::Authentication(
                "SSO GetRoleCredentials response is missing roleCredentials".into(),
            )
        })?;
    let field = |key: &str| role.get(key).and_then(serde_json::Value::as_str);
    let access_key_id = field("accessKeyId").ok_or_else(|| {
        AiError::Authentication("SSO role credentials are missing accessKeyId".into())
    })?;
    let secret_access_key = field("secretAccessKey").ok_or_else(|| {
        AiError::Authentication("SSO role credentials are missing secretAccessKey".into())
    })?;
    let session_token = field("sessionToken").map(str::to_string);
    Ok(ResolvedBedrockAuth {
        credentials: AwsCredentials {
            access_key_id: access_key_id.to_string(),
            secret_access_key: secret_access_key.to_string(),
            session_token,
            source: "sso".to_string(),
        },
        region: profile_region(entries).or(Some(sso_region)),
    })
}

/// Read a region from the process environment (present and non-empty only).
fn region_from_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Split a `credential_process` command line into program + arguments
/// without a shell: whitespace-separated, with double and single quotes
/// grouping. The SDK executes the value directly (the user's own shared
/// config is the trust boundary), so no expansion or shell interpretation
/// happens here either. `None` on an empty value or an unterminated quote.
pub fn split_command_line(command: &str) -> Option<Vec<String>> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut has_current = false;
    let mut quote: Option<char> = None;
    let mut characters = command.chars().peekable();
    while let Some(character) = characters.next() {
        if let Some(open) = quote {
            if character == open {
                // A quote closes its section only at a word boundary (next
                // char is whitespace or end of input); otherwise it is a
                // literal inside the quoted text, so embedded quotes like
                // `print('{"a": 1}')` survive the split.
                match characters.peek() {
                    None => quote = None,
                    Some(next) if next.is_whitespace() => quote = None,
                    Some(_) => current.push(character),
                }
            } else {
                current.push(character);
            }
        } else if character == '"' || character == '\'' {
            quote = Some(character);
            has_current = true;
        } else if character.is_whitespace() {
            if has_current {
                parts.push(std::mem::take(&mut current));
                has_current = false;
            }
        } else {
            current.push(character);
            has_current = true;
        }
    }
    if quote.is_some() {
        return None;
    }
    if has_current {
        parts.push(current);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

/// Run a profile's `credential_process` helper and parse its stdout JSON
/// (`{AccessKeyId, SecretAccessKey, SessionToken?, Expiration?, Version=1}`).
/// The command is spawned directly (no shell, multi-argument form), bounded
/// by [`CREDENTIAL_PROCESS_TIMEOUT`], with stdout/stderr under the shared
/// response-body cap. The child is killed on drop, so no early-return path
/// (timeout, capped stream, parse error) can leak a running helper. Fail-
/// closed: spawn errors, non-zero exits, timeouts, and malformed output are
/// all typed errors.
pub async fn run_credential_process(command: &str) -> AiResult<AwsCredentials> {
    let parts = split_command_line(command).ok_or_else(|| {
        AiError::Authentication(format!(
            "credential_process '{command}' could not be split into program and arguments \
             (is a quote unterminated?)"
        ))
    })?;
    let program = parts[0].clone();
    let mut child = tokio::process::Command::new(&program)
        .args(&parts[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // RAII teardown: every drop path kills the helper process, so the
        // explicit start_kill calls below only pin down WHEN it dies on the
        // in-block error paths.
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            AiError::Authentication(format!(
                "failed to spawn credential_process '{program}': {error}"
            ))
        })?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| AiError::Authentication("credential_process stdout unavailable".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| AiError::Authentication("credential_process stderr unavailable".into()))?;
    let outcome = tokio::time::timeout(CREDENTIAL_PROCESS_TIMEOUT, async {
        let out = read_capped_stream(&mut stdout, "credential_process stdout").await?;
        let err = read_capped_stream(&mut stderr, "credential_process stderr").await?;
        let status = child.wait().await.map_err(|error| {
            AiError::Authentication(format!("credential_process failed: {error}"))
        })?;
        Ok::<_, AiError>((out, err, status))
    })
    .await;
    match outcome {
        Err(_) => {
            child.start_kill().ok();
            Err(AiError::Authentication(format!(
                "credential_process '{program}' timed out after {}s",
                CREDENTIAL_PROCESS_TIMEOUT.as_secs()
            )))
        }
        Ok(Err(error)) => {
            child.start_kill().ok();
            Err(error)
        }
        Ok(Ok((out, err, status))) => {
            if !status.success() {
                let code = match status.code() {
                    Some(code) => format!("exit code {code}"),
                    None => "a non-zero exit".to_string(),
                };
                let stderr_excerpt = truncate_body(&String::from_utf8_lossy(&err));
                return Err(AiError::Authentication(format!(
                    "credential_process '{program}' failed with {code}: {stderr_excerpt}"
                )));
            }
            parse_credential_process_output(&String::from_utf8_lossy(&out))
        }
    }
}

/// Read a child-process stream under the shared response-body cap.
async fn read_capped_stream<R>(stream: &mut R, what: &str) -> AiResult<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = stream
            .read(&mut buffer)
            .await
            .map_err(|error| AiError::Authentication(format!("reading {what} failed: {error}")))?;
        if read == 0 {
            break;
        }
        if bytes.len() as u64 + read as u64 > MAX_RESPONSE_BODY_BYTES {
            return Err(AiError::Authentication(format!(
                "{what} exceeds the {MAX_RESPONSE_BODY_BYTES}-byte cap"
            )));
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(bytes)
}

/// Parse a `credential_process` stdout payload. `Version`, when present, must
/// be `1`; `AccessKeyId`/`SecretAccessKey` are required and non-empty;
/// `SessionToken` and `Expiration` are optional (`Expiration` is carried
/// opaquely — pir signs immediately and does not cache across calls).
pub fn parse_credential_process_output(body: &str) -> AiResult<AwsCredentials> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|error| {
        AiError::Authentication(format!(
            "credential_process output was not valid JSON: {error}"
        ))
    })?;
    if let Some(version) = value.get("Version") {
        let supported = version.as_i64() == Some(1) || version.as_str() == Some("1");
        if !supported {
            return Err(AiError::Authentication(format!(
                "credential_process output declares Version {version}; only Version 1 \
                 is supported"
            )));
        }
    }
    let field = |name: &str| -> Option<String> {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    match (field("AccessKeyId"), field("SecretAccessKey")) {
        (Some(access_key_id), Some(secret_access_key)) => Ok(AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token: field("SessionToken"),
            source: "credential-process".to_string(),
        }),
        _ => Err(AiError::Authentication(
            "credential_process output is missing AccessKeyId or SecretAccessKey".to_string(),
        )),
    }
}

/// SigV4-signed `sts:AssumeRole` using the source profile's base credentials
/// (unlike `AssumeRoleWithWebIdentity`, this call requires a signature, so it
/// reuses the Bedrock adapter's SigV4 implementation with service `sts`).
/// Public (and endpoint/region-configurable) so the independent test binary
/// can drive it against a local mock.
pub async fn sts_assume_role(
    endpoint: &str,
    region: &str,
    role_arn: &str,
    session_name: &str,
    source: &AwsCredentials,
) -> AiResult<AwsCredentials> {
    let client = credentials_client(STS_TIMEOUT, "STS")?;
    let body = form_urlencode(&[
        ("Action", "AssumeRole"),
        ("Version", "2011-06-15"),
        ("RoleArn", role_arn),
        ("RoleSessionName", session_name),
    ]);
    let headers = BTreeMap::from([
        (
            "content-type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        ),
        ("accept".to_string(), "application/xml".to_string()),
    ]);
    let signed = crate::ai::api::bedrock::sigv4_sign(
        "POST",
        endpoint,
        &headers,
        body.as_bytes(),
        &source.access_key_id,
        &source.secret_access_key,
        source.session_token.as_deref(),
        region,
        "sts",
    )?;
    let mut request = client.post(endpoint).body(body);
    for (name, value) in signed {
        request = request.header(name, value);
    }
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| AiError::Http(format!("STS AssumeRole request failed: {error}")))?;
    let status = response.status().as_u16();
    let body = read_bounded_body(response, "STS AssumeRole").await?;
    if !(200..300).contains(&status) {
        return Err(AiError::Authentication(format!(
            "STS AssumeRole failed (HTTP {status}): {}",
            truncate_body(&body)
        )));
    }
    let mut credentials = parse_sts_credentials(&body)?;
    credentials.source = "assume-role".to_string();
    Ok(credentials)
}

/// `application/x-www-form-urlencoded` body serialization (percent-encoding,
/// matching the SigV4 canonical form; `%20` for spaces is accepted by STS).
fn form_urlencode(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| {
            format!(
                "{}={}",
                form_urlencode_component(name),
                form_urlencode_component(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn form_urlencode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// EC2 instance metadata (IMDSv2)
// ---------------------------------------------------------------------------

/// Whether the IMDS link is explicitly disabled
/// (`AWS_EC2_METADATA_DISABLED=true|1`).
fn imds_disabled() -> bool {
    std::env::var("AWS_EC2_METADATA_DISABLED")
        .ok()
        .is_some_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "true" | "1"))
}

/// The IMDS endpoint: `AWS_EC2_METADATA_SERVICE_ENDPOINT` override when it
/// points at a loopback/link-local http(s) host (the same restriction the
/// SDK's container-credential endpoint applies), else the default
/// link-local address. `None` = the override is unusable; the link skips.
fn imds_endpoint() -> Option<String> {
    if let Ok(value) = std::env::var("AWS_EC2_METADATA_SERVICE_ENDPOINT") {
        let value = value.trim();
        if !value.is_empty() {
            if (value.starts_with("http://") || value.starts_with("https://"))
                && ecs_full_uri_host_allowed(value)
            {
                return Some(value.trim_end_matches('/').to_string());
            }
            return None;
        }
    }
    Some(IMDS_ENDPOINT.to_string())
}

/// Attempt the EC2 IMDSv2 link: PUT a session token, list the instance role,
/// fetch the role's credentials. This is a chain link, not a terminal error:
/// every failure (not on EC2, timeouts, malformed payloads) resolves to
/// `None` so the chain continues, with the whole flow bounded by
/// [`IMDS_TOTAL_BUDGET`] so non-EC2 environments skip quickly and silently.
pub async fn imds_credentials() -> Option<AwsCredentials> {
    if imds_disabled() {
        return None;
    }
    let endpoint = imds_endpoint()?;
    let attempt = tokio::time::timeout(IMDS_TOTAL_BUDGET, async {
        let client = reqwest::Client::builder()
            .connect_timeout(IMDS_CONNECT_TIMEOUT)
            .timeout(IMDS_REQUEST_TIMEOUT)
            .build()
            .ok()?;
        // 1. IMDSv2 session token (PUT with the requested TTL).
        let token = fetch_imds_text(
            &client,
            reqwest::Method::PUT,
            &format!("{endpoint}/latest/api/token"),
            None,
        )
        .await?;
        let token = token.trim();
        if token.is_empty() || token.len() > 2048 {
            return None;
        }
        // 2. The first instance role name (a role name is a simple token;
        // anything else is not a listing we trust).
        let listing = fetch_imds_text(
            &client,
            reqwest::Method::GET,
            &format!("{endpoint}/latest/meta-data/iam/security-credentials/"),
            Some(token),
        )
        .await?;
        let role = listing
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())?;
        if role.len() > 256
            || !role.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
        {
            return None;
        }
        // 3. The role's credentials document.
        let body = fetch_imds_text(
            &client,
            reqwest::Method::GET,
            &format!("{endpoint}/latest/meta-data/iam/security-credentials/{role}"),
            Some(token),
        )
        .await?;
        parse_imds_credentials(&body).ok()
    })
    .await
    .ok()?;
    attempt
}

/// One IMDS request with the token header when supplied; the token PUT also
/// carries the TTL request header (IMDSv2 requires it — without it real
/// IMDS answers 400); any failure (status or body) collapses to `None` so
/// the link stays a silent skip.
async fn fetch_imds_text(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    token: Option<&str>,
) -> Option<String> {
    let mut request = client.request(method.clone(), url);
    if let Some(token) = token {
        request = request.header("x-aws-ec2-metadata-token", token);
    } else if method == reqwest::Method::PUT {
        request = request.header(
            "x-aws-ec2-metadata-token-ttl-seconds",
            IMDS_TOKEN_TTL_SECONDS,
        );
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    read_bounded_body(response, "IMDS").await.ok()
}

/// Parse an IMDS role-credentials document
/// (`{AccessKeyId, SecretAccessKey, Token, Expiration}`).
pub fn parse_imds_credentials(body: &str) -> AiResult<AwsCredentials> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|error| {
        AiError::Authentication(format!(
            "IMDS credentials response was not valid JSON: {error}"
        ))
    })?;
    let field = |name: &str| -> Option<String> {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    match (
        field("AccessKeyId"),
        field("SecretAccessKey"),
        field("Token"),
    ) {
        (Some(access_key_id), Some(secret_access_key), Some(token)) => Ok(AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token: Some(token),
            source: "ec2-imds".to_string(),
        }),
        _ => Err(AiError::Authentication(
            "IMDS credentials response is missing AccessKeyId, SecretAccessKey, or Token"
                .to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Web identity (sts:AssumeRoleWithWebIdentity)
// ---------------------------------------------------------------------------

/// Resolve credentials from a web identity token file. The STS call is
/// anonymous (no signing), so it rides the plain reqwest client with a
/// bounded timeout.
pub async fn web_identity_credentials(env: Option<&ProviderEnv>) -> AiResult<ResolvedBedrockAuth> {
    let token_file = provider_env_value("AWS_WEB_IDENTITY_TOKEN_FILE", env)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AiError::Authentication("AWS_WEB_IDENTITY_TOKEN_FILE was not set".to_string())
        })?;
    let role_arn = provider_env_value("AWS_ROLE_ARN", env)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AiError::Authentication(
                "AWS_WEB_IDENTITY_TOKEN_FILE is set but AWS_ROLE_ARN is missing".to_string(),
            )
        })?;
    let session_name =
        provider_env_value("AWS_ROLE_SESSION_NAME", env).unwrap_or_else(|| "pir".to_string());
    let token = std::fs::read_to_string(&token_file).map_err(|error| {
        AiError::Authentication(format!(
            "failed to read AWS web identity token file '{token_file}': {error}"
        ))
    })?;
    let token = token.trim();
    if token.is_empty() {
        return Err(AiError::Authentication(format!(
            "AWS web identity token file '{token_file}' is empty"
        )));
    }
    // The STS endpoint uses the same region selection as the SDK default
    // (AWS_REGION / AWS_DEFAULT_REGION, then us-east-1); it is independent of
    // the Bedrock signing region.
    let region = provider_env_value("AWS_REGION", env)
        .or_else(|| provider_env_value("AWS_DEFAULT_REGION", env))
        .unwrap_or_else(|| "us-east-1".to_string());
    if !is_valid_sts_region(&region) {
        return Err(AiError::Authentication(format!(
            "AWS_REGION '{region}' is not a valid AWS region (expected [a-z0-9-]+); \
             refusing to build the STS endpoint"
        )));
    }
    let credentials =
        sts_assume_role_with_web_identity(&sts_endpoint(&region), &role_arn, &session_name, token)
            .await?;
    Ok(ResolvedBedrockAuth {
        credentials: AwsCredentials {
            source: "web-identity".to_string(),
            ..credentials
        },
        region: None,
    })
}

/// The regional STS endpoint (the `.cn` partition gets the `.com.cn` suffix).
/// Callers must validate the region with [`is_valid_sts_region`] first: the
/// region is interpolated into the endpoint hostname, so an unvalidated value
/// could redirect the token-bearing POST to an external host.
pub fn sts_endpoint(region: &str) -> String {
    if region.starts_with("cn-") {
        format!("https://sts.{region}.amazonaws.com.cn")
    } else {
        format!("https://sts.{region}.amazonaws.com")
    }
}

/// Whether a region string is safe to interpolate into the STS endpoint
/// hostname (`^[a-z0-9-]+$`). Anything else — dots, `#`, slashes, empty — is
/// rejected so a hostile `AWS_REGION=foo.attacker.com#` cannot aim the
/// web-identity POST (which carries the token) off the AWS endpoints.
pub fn is_valid_sts_region(region: &str) -> bool {
    !region.is_empty()
        && region.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

/// POST the anonymous `AssumeRoleWithWebIdentity` form call and parse the XML
/// response. `endpoint` is configurable so tests/ can point at a local mock.
pub async fn sts_assume_role_with_web_identity(
    endpoint: &str,
    role_arn: &str,
    session_name: &str,
    web_identity_token: &str,
) -> AiResult<AwsCredentials> {
    let client = credentials_client(STS_TIMEOUT, "STS")?;
    let fields = [
        ("Action", "AssumeRoleWithWebIdentity"),
        ("Version", "2011-06-15"),
        ("RoleArn", role_arn),
        ("RoleSessionName", session_name),
        ("WebIdentityToken", web_identity_token),
    ];
    let response = client
        .post(endpoint)
        .header("accept", "application/xml")
        .form(&fields)
        .send()
        .await
        .map_err(|error| {
            AiError::Http(format!(
                "STS AssumeRoleWithWebIdentity request failed: {error}"
            ))
        })?;
    let status = response.status().as_u16();
    let body = read_bounded_body(response, "STS AssumeRoleWithWebIdentity").await?;
    if !(200..300).contains(&status) {
        return Err(AiError::Authentication(format!(
            "STS AssumeRoleWithWebIdentity failed (HTTP {status}): {}",
            truncate_body(&body)
        )));
    }
    let mut credentials = parse_sts_credentials(&body)?;
    credentials.source = "web-identity".to_string();
    Ok(credentials)
}

/// Precompiled matchers for the STS XML field extraction (the tags are fixed,
/// so each pattern compiles once per process instead of per field per
/// response). The original per-compile error message is preserved lazily in
/// the (unreachable in practice) case a fixed pattern fails to build.
struct StsFieldPatterns {
    access_key_id: Result<Regex, String>,
    secret_access_key: Result<Regex, String>,
    session_token: Result<Regex, String>,
}

fn sts_field_regex(tag: &str) -> Result<Regex, String> {
    let pattern = format!(r"(?s)<[A-Za-z0-9:]*{tag}>([^<]*)</[A-Za-z0-9:]*{tag}>");
    Regex::new(&pattern).map_err(|error| format!("STS response parser error: {error}"))
}

fn sts_field_patterns() -> &'static StsFieldPatterns {
    static PATTERNS: std::sync::OnceLock<StsFieldPatterns> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| StsFieldPatterns {
        access_key_id: sts_field_regex("AccessKeyId"),
        secret_access_key: sts_field_regex("SecretAccessKey"),
        session_token: sts_field_regex("SessionToken"),
    })
}

/// Extract the temporary credentials from an `AssumeRoleWithWebIdentity`
/// XML response (namespace-tolerant tag matching over `<Credentials>`).
pub fn parse_sts_credentials(xml: &str) -> AiResult<AwsCredentials> {
    let patterns = sts_field_patterns();
    let field = |compiled: &Result<Regex, String>| -> AiResult<Option<String>> {
        let re = compiled
            .as_ref()
            .map_err(|message| AiError::Http(message.clone()))?;
        Ok(re
            .captures(xml)
            .map(|captures| captures[1].trim().to_string())
            .filter(|value| !value.is_empty()))
    };
    let access_key_id = field(&patterns.access_key_id)?;
    let secret_access_key = field(&patterns.secret_access_key)?;
    let session_token = field(&patterns.session_token)?;
    match (access_key_id, secret_access_key, session_token) {
        (Some(access), Some(secret), Some(session)) if !access.is_empty() && !secret.is_empty() => {
            Ok(AwsCredentials {
                access_key_id: access,
                secret_access_key: secret,
                session_token: Some(session),
                source: "web-identity".to_string(),
            })
        }
        _ => Err(AiError::Authentication(
            "STS AssumeRoleWithWebIdentity response did not contain a complete <Credentials> \
             block (AccessKeyId, SecretAccessKey, SessionToken)"
                .to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// ECS container credentials
// ---------------------------------------------------------------------------

/// Fetch ECS container credentials from the resolved endpoint (relative URIs
/// resolve against the link-local 169.254.170.2 metadata address; full URIs
/// must be http(s) with a loopback/link-local host). When
/// `AWS_CONTAINER_AUTHORIZATION_TOKEN` (or the `_FILE` variant) is
/// configured, the request carries the matching `Authorization` header —
/// EKS and hardened ECS endpoints reject unauthenticated fetches.
/// Fail-closed: non-2xx or malformed payloads are typed errors.
pub async fn fetch_ecs_credentials(url: &str) -> AiResult<AwsCredentials> {
    let client = credentials_client(ECS_TIMEOUT, "ECS credentials")?;
    let mut request = client.get(url).header("accept", "application/json");
    if let Some(token) = ecs_authorization_token()? {
        request = request.header("authorization", token);
    }
    let response = request.send().await.map_err(|error| {
        AiError::Http(format!("ECS container credentials request failed: {error}"))
    })?;
    let status = response.status().as_u16();
    let body = read_bounded_body(response, "ECS container credentials").await?;
    if !(200..300).contains(&status) {
        return Err(AiError::Authentication(format!(
            "ECS container credentials endpoint returned HTTP {status}: {}",
            truncate_body(&body)
        )));
    }
    parse_ecs_credentials(&body)
}

/// The ECS container-credentials authorization token (P1-3): when
/// `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE` is set, its file contents are
/// read and trimmed (taking precedence, matching the SDK's EKS pod-identity
/// behavior); otherwise `AWS_CONTAINER_AUTHORIZATION_TOKEN` is used verbatim
/// (trimmed). `Ok(None)` = neither variable is configured, so no
/// `Authorization` header is sent. A set-but-unreadable or empty token file
/// is a typed error (fail closed).
pub fn ecs_authorization_token() -> AiResult<Option<String>> {
    if let Ok(path) = std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
        let path = path.trim();
        if !path.is_empty() {
            let token = std::fs::read_to_string(path).map_err(|error| {
                AiError::Authentication(format!(
                    "failed to read AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE '{path}': {error}"
                ))
            })?;
            let token = token.trim();
            if token.is_empty() {
                return Err(AiError::Authentication(format!(
                    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE '{path}' is empty"
                )));
            }
            return Ok(Some(token.to_string()));
        }
    }
    Ok(std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN")
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty()))
}

/// Parse the ECS credentials payload
/// (`{AccessKeyId, SecretAccessKey, Token, Expiration}`). `Expiration` is
/// carried opaquely: pir signs immediately and does not cache across calls.
pub fn parse_ecs_credentials(body: &str) -> AiResult<AwsCredentials> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|error| {
        AiError::Authentication(format!(
            "ECS container credentials response was not valid JSON: {error}"
        ))
    })?;
    let required = |field: &str| -> Option<String> {
        value
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let access_key_id = required("AccessKeyId");
    let secret_access_key = required("SecretAccessKey");
    match (access_key_id, secret_access_key) {
        (Some(access), Some(secret)) => Ok(AwsCredentials {
            access_key_id: access,
            secret_access_key: secret,
            session_token: required("Token"),
            source: "ecs-container".to_string(),
        }),
        _ => Err(AiError::Authentication(
            "ECS container credentials response is missing AccessKeyId or SecretAccessKey"
                .to_string(),
        )),
    }
}

/// Bound an error-body excerpt (no secrets past the cap either way).
fn truncate_body(body: &str) -> String {
    body.chars().take(200).collect()
}

/// The bounded HTTPS client for the AWS credential endpoints: request +
/// connect timeouts, with `what` labeling the typed build-failure error.
fn credentials_client(timeout: Duration, what: &str) -> AiResult<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|error| AiError::Http(format!("{what} client build failed: {error}")))
}

/// Read a response body under a hard size cap: an oversized `Content-Length`
/// is rejected before any read, and the streamed read stops at
/// [`MAX_RESPONSE_BODY_BYTES`] even when the header lies. Both limits are
/// typed errors (fail closed); the read keeps the bounded-timeout client.
async fn read_bounded_body(mut response: reqwest::Response, what: &str) -> AiResult<String> {
    if let Some(length) = response.content_length() {
        if length > MAX_RESPONSE_BODY_BYTES {
            return Err(AiError::Http(format!(
                "{what} response Content-Length {length} exceeds the \
                 {MAX_RESPONSE_BODY_BYTES}-byte cap"
            )));
        }
    }
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        AiError::Http(format!("reading the {what} response body failed: {error}"))
    })? {
        if bytes.len() as u64 + chunk.len() as u64 > MAX_RESPONSE_BODY_BYTES {
            return Err(AiError::Http(format!(
                "{what} response body exceeds the {MAX_RESPONSE_BODY_BYTES}-byte cap"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|error| {
        AiError::Http(format!(
            "the {what} response body was not valid UTF-8: {error}"
        ))
    })
}
