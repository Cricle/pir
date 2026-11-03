//! P2A process-level credential-cache tests (R1-L2-006).
//!
//! The cache itself is exercised by unit tests inside
//! `src/ai/api/bedrock/credentials.rs` (fake clock, injectable resolver,
//! single-flight and expiry refresh). These integration tests pin the public
//! entry point: `resolve_credentials` must be stable across repeated calls
//! for one effective environment, must not leak an entry into a request with
//! different AWS inputs, and must not cache failures.

use pir::ai::api::bedrock::credentials::resolve_credentials;
use pir::ai::types::ProviderEnv;

/// A request-level environment overlay that shadows every source-selecting
/// process variable, so the test is independent of the developer machine's
/// AWS configuration.
fn static_env(access_key_id: &str, secret_access_key: &str) -> ProviderEnv {
    let mut env = ProviderEnv::new();
    for name in [
        "AWS_PROFILE",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_ROLE_ARN",
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    ] {
        env.insert(name.to_string(), String::new());
    }
    env.insert("AWS_ACCESS_KEY_ID".to_string(), access_key_id.to_string());
    env.insert(
        "AWS_SECRET_ACCESS_KEY".to_string(),
        secret_access_key.to_string(),
    );
    env
}

#[tokio::test]
async fn repeated_resolution_of_the_same_environment_is_stable() {
    let env = static_env("AKID-P2A-A", "secret-p2a-a");
    let first = resolve_credentials(Some(&env))
        .await
        .expect("static env resolves");
    let second = resolve_credentials(Some(&env))
        .await
        .expect("cached static env resolves");
    assert_eq!(first, second, "the cache must return the same credentials");
    assert_eq!(first.credentials.access_key_id, "AKID-P2A-A");
    assert_eq!(first.credentials.source, "environment");
}

#[tokio::test]
async fn a_different_effective_environment_does_not_reuse_the_entry() {
    let env_a = static_env("AKID-P2A-A", "secret-p2a-a");
    let env_b = static_env("AKID-P2A-B", "secret-p2a-b");
    let a = resolve_credentials(Some(&env_a))
        .await
        .expect("environment A resolves");
    let b = resolve_credentials(Some(&env_b))
        .await
        .expect("environment B resolves");
    assert_eq!(a.credentials.access_key_id, "AKID-P2A-A");
    assert_eq!(
        b.credentials.access_key_id, "AKID-P2A-B",
        "the cache key must include the effective AWS env"
    );
}

#[tokio::test]
async fn incomplete_static_env_failures_are_not_cached() {
    let mut env = static_env("AKID-P2A-INCOMPLETE", "");
    env.remove("AWS_SECRET_ACCESS_KEY");
    for _ in 0..2 {
        let error = resolve_credentials(Some(&env))
            .await
            .expect_err("an incomplete static env must fail closed");
        assert!(
            error.to_string().contains("must both be set"),
            "unexpected error: {error}"
        );
    }
}
