//! Test surface for the Bedrock credential cache.
//!
//! The cache tests are white-box: they inject a clock, drive the single-flight
//! cells and call four credential parsers directly. This module re-exposes
//! exactly those seams behind **opaque wrappers** — the cache, its key and the
//! resolved value stay private types inside the crate, so nothing here widens
//! the real API surface.
//!
//! Shape follows `pir-telemetry::testing` (one named module holding the whole
//! test surface) rather than scattering `__test_*` functions through
//! production code.

use std::time::{Duration, SystemTime};

use super::{
    AwsCredentials, CredentialCache, CredentialCacheKey, ResolvedBedrockAuth, ResolvedCredential,
};
use crate::ai::error::AiResult;

/// A cache with an injected clock, TTL and refresh margin.
pub struct Cache(CredentialCache);

/// One cache key (an `AWS_PROFILE` value).
pub struct Key(CredentialCacheKey);

/// What the tests can read back from a successful resolution.
pub struct Resolved {
    pub access_key_id: String,
    pub region: Option<String>,
}

impl Cache {
    /// Drive the single-flight lookup. `resolve` returns the access key id and
    /// the absolute expiration the source reported.
    pub async fn resolve<F, Fut>(&self, key: &Key, resolve: F) -> AiResult<Resolved>
    where
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = AiResult<(String, Option<SystemTime>)>> + Send,
    {
        let key = Key(key.0.clone());
        let auth: ResolvedBedrockAuth = self
            .0
            .get_or_resolve(key.0, move || async move {
                let (access_key_id, expiration) = resolve().await?;
                Ok(ResolvedCredential::new(
                    AwsCredentials {
                        access_key_id,
                        secret_access_key: "secret".to_string(),
                        session_token: None,
                        source: "test".to_string(),
                    },
                    None,
                    expiration,
                ))
            })
            .await?;
        Ok(Resolved {
            access_key_id: auth.credentials.access_key_id,
            region: auth.region,
        })
    }
}

/// A cache with the clock, TTL and refresh margin the tests inject.
pub fn cache(
    clock: impl Fn() -> SystemTime + Send + Sync + 'static,
    ttl: Duration,
    refresh_margin: Duration,
) -> Cache {
    Cache(CredentialCache::new(clock, ttl, refresh_margin))
}

/// One cache key, keyed on an `AWS_PROFILE` value.
pub fn key(profile: &str) -> Key {
    Key(CredentialCacheKey(vec![(
        "AWS_PROFILE",
        Some(profile.to_string()),
    )]))
}

/// A key is cheap to clone; the tests reuse one across calls.
impl Clone for Key {
    fn clone(&self) -> Self {
        Key(self.0.clone())
    }
}

/// The expiration each of the four expiring parsers captured.
pub fn ecs_expiration(body: &str) -> AiResult<Option<SystemTime>> {
    super::parse_ecs_credentials_expiring(body).map(|creds| creds.expiration)
}

pub fn credential_process_expiration(body: &str) -> AiResult<Option<SystemTime>> {
    super::parse_credential_process_output_expiring(body).map(|creds| creds.expiration)
}

pub fn sts_expiration(xml: &str) -> AiResult<Option<SystemTime>> {
    super::parse_sts_credentials_expiring(xml).map(|creds| creds.expiration)
}

pub fn imds_expiration(body: &str) -> AiResult<Option<SystemTime>> {
    super::parse_imds_credentials_expiring(body).map(|creds| creds.expiration)
}

/// The ISO-8601 parser fixtures build their expected timestamps with.
pub fn iso8601(value: &str) -> Option<SystemTime> {
    super::system_time_from_iso8601(value)
}
