//! OAuth infrastructure integration tests: PKCE vectors, token exchange
//! against a local mock server (no live network), the file credential store
//! roundtrip with env precedence, and the cancel-safe refresh rotation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use pir::ai::auth::oauth::anthropic;
use pir::ai::auth::{
    oauth_expires_soon, refresh_stored_oauth_credential, resolve_stored_auth, FileAuthStorage,
    LoginOptions, OAuthCredential, OAuthFlow, StoredCredential,
};
use pir::ai::error::{AiError, AiResult};
use pir::ai::types::{ApiKeyCredential, AuthResult, ModelAuth};
use serde_json::json;

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

#[test]
fn pkce_rfc7636_vector() {
    let pkce = pir::ai::auth::pkce::generate_pkce();
    let _ = pkce; // covered by unit tests; smoke the API surface here
    let challenge =
        pir::ai::auth::pkce::challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
}

// ---------------------------------------------------------------------------
// Mock token endpoint
// ---------------------------------------------------------------------------

/// One-shot HTTP server bound to an ephemeral loopback port serving a canned
/// response (hand-rolled like the callback server; no live network).
async fn serve_one_response(response: String) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buffer = [0u8; 4096];
        let _ = stream.read(&mut buffer).await;
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    });
    (port, handle)
}

#[tokio::test]
async fn token_exchange_against_mock_server() {
    let body = json!({
        "access_token": "access-1",
        "refresh_token": "refresh-1",
        "expires_in": 3600
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, server) = serve_one_response(response).await;

    let result = pir::ai::auth::http::post_json(
        &format!("http://127.0.0.1:{port}/v1/oauth/token"),
        &json!({
            "grant_type": "authorization_code",
            "client_id": anthropic::CLIENT_ID,
            "code": "abc",
            "state": "def",
            "redirect_uri": "http://localhost:53692/callback",
            "code_verifier": "verifier",
        }),
    )
    .await
    .unwrap();
    server.await.unwrap();

    assert!(result.ok(), "status {}", result.status);
    let credential = anthropic::credential_from_token(&result.json().unwrap()).unwrap();
    assert_eq!(credential.access, "access-1");
    assert_eq!(credential.refresh, "refresh-1");
    // Upstream subtracts the 5-minute safety margin from expires_in.
    let now = pir::ai::util::now_ms() as i64;
    let remaining = credential.expires - now;
    assert!(
        remaining > 45 * 60 * 1000 && remaining <= 55 * 60 * 1000,
        "remaining {remaining}ms should be ~55min"
    );
}

#[tokio::test]
async fn token_exchange_error_maps_to_message() {
    let body = json!({
        "error": "invalid_grant",
        "error_description": "code expired"
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, server) = serve_one_response(response).await;
    let result = pir::ai::auth::http::post_form(
        &format!("http://127.0.0.1:{port}/token"),
        &[("grant_type", "refresh_token")],
    )
    .await
    .unwrap();
    server.await.unwrap();
    assert!(!result.ok());
    let detail = pir::ai::auth::http::error_detail(&result.json().unwrap());
    assert_eq!(detail.as_deref(), Some("invalid_grant: code expired"));
}

// ---------------------------------------------------------------------------
// Credential store roundtrip + precedence over env
// ---------------------------------------------------------------------------

fn temp_storage(name: &str) -> (PathGuard, Arc<FileAuthStorage>) {
    let dir = std::env::temp_dir().join(format!(
        "pir-auth-oauth-test-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let storage = Arc::new(FileAuthStorage::new(dir.join(name)));
    (PathGuard(dir), storage)
}

struct PathGuard(std::path::PathBuf);

impl Drop for PathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fake flow with canned refresh/toAuth behavior (no network).
#[derive(Default)]
struct FakeFlow {
    /// When set, refresh fails with this message.
    fail_refresh: std::sync::Mutex<Option<String>>,
    /// Extra delay inside refresh (to exercise caller cancellation).
    refresh_delay: std::sync::Mutex<Option<Duration>>,
}

impl OAuthFlow for FakeFlow {
    fn name(&self) -> &str {
        "Fake"
    }

    fn login<'a>(
        &'a self,
        _interaction: &'a dyn pir::ai::auth::AuthInteraction,
        _options: &'a LoginOptions,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<OAuthCredential>> + Send + 'a>>
    {
        Box::pin(async { Ok(OAuthCredential::new("a", "r", 0)) })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<OAuthCredential>> + Send + 'a>>
    {
        Box::pin(async move {
            let delay = *self.refresh_delay.lock().unwrap();
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let failure = self.fail_refresh.lock().unwrap().clone();
            if let Some(message) = failure {
                return Err(AiError::Authentication(message));
            }
            // Rotation: the refresh token changes on every refresh.
            let mut rotated = OAuthCredential::new(
                format!("{}-rotated", credential.access),
                format!("{}-rotated", credential.refresh),
                pir::ai::util::now_ms() as i64 + 3_600_000,
            );
            rotated.set_extra("rotated", Some(json!(true)));
            Ok(rotated)
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: None,
        })
    }
}

fn env_context() -> BTreeMap<String, String> {
    BTreeMap::new()
}

#[tokio::test]
async fn stored_oauth_precedes_env_and_roundtrips() {
    let (_guard, storage) = temp_storage("auth.json");
    storage
        .store(
            "anthropic",
            StoredCredential::OAuth(OAuthCredential::new(
                "oauth-access",
                "oauth-refresh",
                pir::ai::util::now_ms() as i64 + 3_600_000,
            )),
        )
        .unwrap();

    let flow = pir::ai::auth::oauth_flow_for("anthropic", None).unwrap();
    // A stored OAuth credential owns the provider: the api-key closure (which
    // would resolve the env var) is never consulted.
    let result = resolve_stored_auth(&storage, "anthropic", Some(&flow), &|credential: Option<
        &ApiKeyCredential,
    >|
     -> AiResult<
        Option<AuthResult>,
    > {
        let _ = credential;
        panic!("env fallback must not run when an OAuth credential is stored");
    })
    .await
    .unwrap()
    .expect("oauth credential should resolve");
    assert_eq!(result.source.as_deref(), Some("OAuth"));
    assert_eq!(result.auth.api_key.as_deref(), Some("oauth-access"));
    let _ = env_context();

    // A stored api-key credential wins over the env closure too.
    storage
        .store(
            "openai",
            StoredCredential::ApiKey(ApiKeyCredential::new(Some("stored-key".to_string()))),
        )
        .unwrap();
    let result = resolve_stored_auth(&storage, "openai", None, &|credential: Option<
        &ApiKeyCredential,
    >|
     -> AiResult<
        Option<AuthResult>,
    > {
        Ok(credential.map(|c| AuthResult {
            auth: ModelAuth {
                api_key: c.key.clone(),
                headers: None,
                base_url: None,
            },
            env: None,
            source: Some("stored credential".to_string()),
        }))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.source.as_deref(), Some("stored credential"));
    assert_eq!(result.auth.api_key.as_deref(), Some("stored-key"));
}

#[tokio::test]
async fn expired_oauth_refreshes_and_persists_rotation() {
    let (_guard, storage) = temp_storage("auth.json");
    storage
        .store(
            "fake",
            StoredCredential::OAuth(OAuthCredential::new(
                "old-access",
                "old-refresh",
                // Expired five minutes ago.
                pir::ai::util::now_ms() as i64 - 300_000,
            )),
        )
        .unwrap();
    // A fake flow: no live network in tests.
    let flow: Arc<dyn OAuthFlow> = Arc::new(FakeFlow::default());
    let credential = pir::ai::auth::resolve::resolve_oauth_credential(
        storage.clone(),
        "fake",
        flow,
        OAuthCredential::new("old-access", "old-refresh", 0),
    )
    .await
    .unwrap();
    assert_eq!(credential.access, "old-access-rotated");
    // The rotation was persisted under the store lock.
    let stored = storage.read_stored("fake").unwrap().unwrap();
    let StoredCredential::OAuth(stored) = stored else {
        panic!("expected oauth credential");
    };
    assert_eq!(stored.access, "old-access-rotated");
    assert_eq!(stored.refresh, "old-refresh-rotated");
    assert!(!oauth_expires_soon(&stored, 0));
}

#[tokio::test]
async fn cancelled_caller_does_not_discard_rotation() {
    // Mirrors the upstream invariant: once a refresh starts, cancelling the
    // caller must not lose the rotated refresh token.
    let (_guard, storage) = temp_storage("auth.json");
    let flow: Arc<dyn OAuthFlow> = Arc::new(FakeFlow {
        refresh_delay: std::sync::Mutex::new(Some(Duration::from_millis(300))),
        ..FakeFlow::default()
    });
    storage
        .store(
            "fake",
            StoredCredential::OAuth(OAuthCredential::new(
                "old",
                "old",
                pir::ai::util::now_ms() as i64 - 1_000,
            )),
        )
        .unwrap();
    let refresh = refresh_stored_oauth_credential(storage.clone(), "fake", flow, |credential| {
        pir::ai::util::now_ms() as i64 >= credential.expires
    });
    // The caller gives up after 10ms, while the refresh is still running.
    let _ = tokio::time::timeout(Duration::from_millis(10), refresh).await;
    // Give the detached task time to finish the refresh and persist.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let stored = storage.read_stored("fake").unwrap().unwrap();
    let StoredCredential::OAuth(stored) = stored else {
        panic!("expected oauth credential");
    };
    assert_eq!(stored.access, "old-rotated", "rotation must be persisted");
    assert_eq!(stored.refresh, "old-rotated");
}

#[tokio::test]
async fn fresh_token_is_not_refreshed() {
    let (_guard, storage) = temp_storage("auth.json");
    let flow: Arc<dyn OAuthFlow> = Arc::new(FakeFlow::default());
    let fresh = OAuthCredential::new(
        "fresh",
        "fresh-refresh",
        pir::ai::util::now_ms() as i64 + 3_600_000,
    );
    let resolved = pir::ai::auth::resolve::resolve_oauth_credential(
        storage.clone(),
        "fake",
        flow,
        fresh.clone(),
    )
    .await
    .unwrap();
    assert_eq!(resolved.access, "fresh");
    // Nothing was written back (no refresh happened).
    assert!(storage.read_stored("fake").unwrap().is_none());
}

#[tokio::test]
async fn refresh_failure_propagates() {
    let (_guard, storage) = temp_storage("auth.json");
    storage
        .store(
            "fake",
            StoredCredential::OAuth(OAuthCredential::new(
                "old",
                "old",
                pir::ai::util::now_ms() as i64 - 1_000,
            )),
        )
        .unwrap();
    let flow: Arc<dyn OAuthFlow> = Arc::new(FakeFlow {
        fail_refresh: std::sync::Mutex::new(Some("invalid_grant".to_string())),
        ..FakeFlow::default()
    });
    let error = refresh_stored_oauth_credential(storage.clone(), "fake", flow, |_| true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid_grant"), "{error}");
}
