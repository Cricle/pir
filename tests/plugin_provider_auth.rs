//! Typed native provider-auth seam (`ProviderAuthHooks`): login/refresh/
//! logout wiring, credential-store parity with builtins, precedence over the
//! JS/builtin fallback, failure atomicity, and unregistration.
//!
//! Hermetic: a temp-dir `FileAuthStorage` stands in for `auth.json` and the
//! interactions are in-process fakes; no network, no user config, no plugin
//! host.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::ai::auth::cli::logout_stored;
use pir::ai::auth::{
    login_and_store, oauth_flow_for, refresh_stored_oauth_credential, resolve_stored_auth,
    ApiKeyAuth, AuthEvent, AuthInteraction, AuthPrompt, EnvAuthContext, FileAuthStorage,
    LoginOptions, OAuthCredential, OAuthFlow, StoredCredential,
};
use pir::ai::error::{AiError, AiResult};
use pir::ai::types::ModelAuth;
use pir::plugins::services::{
    AuthPromptContext, ProviderAuth, ProviderAuthError, ProviderAuthHooks, ProviderService,
    ServiceFuture,
};

// ---------------------------------------------------------------------------
// Fakes / fixtures
// ---------------------------------------------------------------------------

/// A service with every `ProviderService` method at its default: the default
/// auth-hook methods are the typed seam embedded/third-party services get.
struct TestProviderService;

impl ProviderService for TestProviderService {}

/// No-op interaction (the fake hooks ignore prompts).
struct TestInteraction;

impl AuthInteraction for TestInteraction {
    fn prompt<'a>(
        &'a self,
        _prompt: AuthPrompt,
    ) -> Pin<Box<dyn Future<Output = AiResult<String>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn notify(&self, _event: AuthEvent) {}
}

/// Fallback flow standing in for the builtin/JS callback. Counts calls so the
/// precedence tests can pin "native wins, JS is not called".
#[derive(Default)]
struct CountingFlow {
    login_calls: AtomicUsize,
    refresh_calls: AtomicUsize,
}

impl CountingFlow {
    fn login_count(&self) -> usize {
        self.login_calls.load(Ordering::SeqCst)
    }

    fn refresh_count(&self) -> usize {
        self.refresh_calls.load(Ordering::SeqCst)
    }
}

impl OAuthFlow for CountingFlow {
    fn name(&self) -> &str {
        "fallback"
    }

    fn login<'a>(
        &'a self,
        _interaction: &'a dyn AuthInteraction,
        _options: &'a LoginOptions,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        self.login_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(OAuthCredential::new(
                "fallback-access",
                "fallback-refresh",
                now_ms() + 3_600_000,
            ))
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>> {
        self.refresh_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(OAuthCredential::new(
                format!("{}-fallback", credential.access),
                format!("{}-fallback", credential.refresh),
                now_ms() + 3_600_000,
            ))
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

/// Scripted native hooks with call counters.
#[derive(Default)]
struct FakeHooks {
    login_auth: Mutex<Option<ProviderAuth>>,
    login_error: Mutex<Option<ProviderAuthError>>,
    refresh_auth: Mutex<Option<ProviderAuth>>,
    refresh_error: Mutex<Option<ProviderAuthError>>,
    logout_error: Mutex<Option<ProviderAuthError>>,
    login_calls: AtomicUsize,
    refresh_calls: AtomicUsize,
    logout_calls: AtomicUsize,
}

impl FakeHooks {
    fn login_ok(auth: ProviderAuth) -> Self {
        Self {
            login_auth: Mutex::new(Some(auth)),
            ..Self::default()
        }
    }

    fn login_failed(message: &str) -> Self {
        Self {
            login_error: Mutex::new(Some(ProviderAuthError::Failed(message.to_string()))),
            ..Self::default()
        }
    }

    fn refresh_ok(auth: ProviderAuth) -> Self {
        Self {
            refresh_auth: Mutex::new(Some(auth)),
            ..Self::default()
        }
    }

    fn refresh_failed(message: &str) -> Self {
        Self {
            refresh_error: Mutex::new(Some(ProviderAuthError::Failed(message.to_string()))),
            ..Self::default()
        }
    }

    fn counts(&self) -> (usize, usize, usize) {
        (
            self.login_calls.load(Ordering::SeqCst),
            self.refresh_calls.load(Ordering::SeqCst),
            self.logout_calls.load(Ordering::SeqCst),
        )
    }
}

impl ProviderAuthHooks for FakeHooks {
    fn login<'a>(
        &'a self,
        _context: AuthPromptContext<'a>,
    ) -> ServiceFuture<'a, Result<ProviderAuth, ProviderAuthError>> {
        self.login_calls.fetch_add(1, Ordering::SeqCst);
        let auth = self.login_auth.lock().unwrap().clone();
        let error = self.login_error.lock().unwrap().clone();
        Box::pin(async move {
            match error {
                Some(error) => Err(error),
                None => auth.ok_or(ProviderAuthError::Unavailable),
            }
        })
    }

    fn refresh(
        &self,
        _current: &ProviderAuth,
    ) -> ServiceFuture<'static, Result<ProviderAuth, ProviderAuthError>> {
        self.refresh_calls.fetch_add(1, Ordering::SeqCst);
        let auth = self.refresh_auth.lock().unwrap().clone();
        let error = self.refresh_error.lock().unwrap().clone();
        Box::pin(async move {
            match error {
                Some(error) => Err(error),
                None => auth.ok_or(ProviderAuthError::Unavailable),
            }
        })
    }

    fn logout(&self) -> ServiceFuture<'static, Result<(), ProviderAuthError>> {
        self.logout_calls.fetch_add(1, Ordering::SeqCst);
        let error = self.logout_error.lock().unwrap().clone();
        Box::pin(async move {
            match error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }
}

fn now_ms() -> i64 {
    pir::ai::util::now_ms() as i64
}

/// Temp directory + storage; removed on drop.
struct TestDir(PathBuf);

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_storage(name: &str) -> (TestDir, Arc<FileAuthStorage>) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-native-provider-auth-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let storage = Arc::new(FileAuthStorage::new(dir.join("auth.json")));
    (TestDir(dir), storage)
}

fn stored_oauth(storage: &FileAuthStorage, provider: &str) -> OAuthCredential {
    match storage.read_stored(provider).unwrap() {
        Some(StoredCredential::OAuth(credential)) => credential,
        other => panic!("expected a stored OAuth credential, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

#[tokio::test]
async fn native_login_stores_key_and_provider_is_usable() {
    const PROVIDER: &str = "native-login-test";
    let (_dir, storage) = temp_storage("login.json");
    let service = TestProviderService;
    let hooks = Arc::new(FakeHooks::login_ok(ProviderAuth::api_key("native-key")));
    service.register_auth_hooks(PROVIDER, hooks.clone());

    let fallback = CountingFlow::default();
    let interaction = TestInteraction;
    let options = LoginOptions::default();
    let auth = login_and_store(
        storage.as_ref(),
        PROVIDER,
        &fallback,
        &interaction,
        &options,
    )
    .await
    .expect("native login succeeds");

    assert_eq!(auth.api_key.as_deref(), Some("native-key"));
    assert_eq!(hooks.counts().0, 1);
    // Native wins: the fallback flow is never called.
    assert_eq!(fallback.login_count(), 0);

    match storage.read_stored(PROVIDER).unwrap() {
        Some(StoredCredential::ApiKey(credential)) => {
            assert_eq!(credential.key.as_deref(), Some("native-key"));
        }
        other => panic!("expected a stored api-key credential, got {other:?}"),
    }

    // The provider is usable: the next auth resolution reads the stored key.
    let resolved = resolve_stored_auth(&storage, PROVIDER, None, &|credential| {
        ApiKeyAuth::new(PROVIDER, Vec::new()).resolve(&EnvAuthContext::new(), credential)
    })
    .await
    .expect("stored credential resolves")
    .expect("provider is configured");
    assert_eq!(resolved.auth.api_key.as_deref(), Some("native-key"));

    service.unregister_auth_hooks(PROVIDER);
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

#[tokio::test]
async fn native_refresh_rotates_and_next_request_uses_new_token() {
    const PROVIDER: &str = "native-refresh-test";
    let (_dir, storage) = temp_storage("refresh.json");
    let old = OAuthCredential::new("old-access", "old-refresh", now_ms() + 3_600_000);
    storage
        .store(PROVIDER, StoredCredential::OAuth(old.clone()))
        .unwrap();

    let service = TestProviderService;
    let hooks = Arc::new(FakeHooks::refresh_ok(ProviderAuth::oauth(
        "new-access",
        "new-refresh",
        now_ms() + 7_200_000,
    )));
    service.register_auth_hooks(PROVIDER, hooks.clone());

    let fallback = Arc::new(CountingFlow::default());
    let refreshed =
        refresh_stored_oauth_credential(storage.clone(), PROVIDER, fallback.clone(), |_| true)
            .await
            .expect("native refresh succeeds")
            .expect("an OAuth credential remains");

    assert_eq!(refreshed.access, "new-access");
    assert_eq!(refreshed.refresh, "new-refresh");
    assert_eq!(hooks.counts().1, 1);
    // Native wins: the fallback `refresh` is never called.
    assert_eq!(fallback.refresh_count(), 0);

    let stored = stored_oauth(storage.as_ref(), PROVIDER);
    assert_eq!(stored.access, "new-access");
    assert_eq!(stored.refresh, "new-refresh");

    // The next request resolves the rotated token from the store. The token
    // is far from expiry, so resolution does not refresh again.
    let flow: pir::ai::auth::OAuthFlowBox = fallback.clone();
    let resolved = resolve_stored_auth(&storage, PROVIDER, Some(&flow), &|_| Ok(None))
        .await
        .expect("rotated credential resolves")
        .expect("provider is configured");
    assert_eq!(resolved.auth.api_key.as_deref(), Some("new-access"));
    assert_eq!(fallback.refresh_count(), 0);

    service.unregister_auth_hooks(PROVIDER);
}

#[tokio::test]
async fn failing_native_refresh_preserves_the_old_credential() {
    const PROVIDER: &str = "native-refresh-failure-test";
    let (_dir, storage) = temp_storage("refresh-failure.json");
    let old = OAuthCredential::new("old-access", "old-refresh", now_ms() + 3_600_000);
    storage
        .store(PROVIDER, StoredCredential::OAuth(old.clone()))
        .unwrap();

    let service = TestProviderService;
    let hooks = Arc::new(FakeHooks::refresh_failed("boom"));
    service.register_auth_hooks(PROVIDER, hooks.clone());

    let fallback = Arc::new(CountingFlow::default());
    let error =
        refresh_stored_oauth_credential(storage.clone(), PROVIDER, fallback.clone(), |_| true)
            .await
            .expect_err("a failing hook is a typed error");

    assert!(
        error.to_string().contains("boom"),
        "unexpected error: {error}"
    );
    // A terminal hook failure does not fall back to the JS/builtin flow.
    assert_eq!(fallback.refresh_count(), 0);
    // The old credential is untouched.
    let stored = stored_oauth(storage.as_ref(), PROVIDER);
    assert_eq!(stored.access, old.access);
    assert_eq!(stored.refresh, old.refresh);

    service.unregister_auth_hooks(PROVIDER);
}

#[tokio::test]
async fn failing_native_login_preserves_the_old_credential() {
    const PROVIDER: &str = "native-login-failure-test";
    let (_dir, storage) = temp_storage("login-failure.json");
    storage
        .store(
            PROVIDER,
            StoredCredential::ApiKey(pir::ai::types::ApiKeyCredential::new(Some(
                "old-key".to_string(),
            ))),
        )
        .unwrap();

    let service = TestProviderService;
    let hooks = Arc::new(FakeHooks::login_failed("denied"));
    service.register_auth_hooks(PROVIDER, hooks.clone());

    let fallback = CountingFlow::default();
    let interaction = TestInteraction;
    let options = LoginOptions::default();
    let error = login_and_store(
        storage.as_ref(),
        PROVIDER,
        &fallback,
        &interaction,
        &options,
    )
    .await
    .expect_err("a failing hook is a typed error");

    assert!(
        error.to_string().contains("denied"),
        "unexpected error: {error}"
    );
    assert_eq!(fallback.login_count(), 0);
    match storage.read_stored(PROVIDER).unwrap() {
        Some(StoredCredential::ApiKey(credential)) => {
            assert_eq!(credential.key.as_deref(), Some("old-key"));
        }
        other => panic!("old credential was modified: {other:?}"),
    }

    service.unregister_auth_hooks(PROVIDER);
}

/// `Unavailable` is the hook's explicit "this flow is not implemented here"
/// signal: the builtin/JS flow runs exactly once, never twice.
#[tokio::test]
async fn unavailable_native_login_falls_back_to_the_flow_once() {
    const PROVIDER: &str = "native-unavailable-fallback-test";
    let (_dir, storage) = temp_storage("unavailable-fallback.json");
    let service = TestProviderService;
    // `FakeHooks::default()` answers `Unavailable` from `login`.
    let hooks = Arc::new(FakeHooks::default());
    service.register_auth_hooks(PROVIDER, hooks.clone());

    let fallback = CountingFlow::default();
    let interaction = TestInteraction;
    let options = LoginOptions::default();
    let auth = login_and_store(
        storage.as_ref(),
        PROVIDER,
        &fallback,
        &interaction,
        &options,
    )
    .await
    .expect("the fallback login succeeds");

    assert_eq!(auth.access_token.as_deref(), Some("fallback-access"));
    assert_eq!(hooks.counts().0, 1);
    assert_eq!(fallback.login_count(), 1);
    match storage.read_stored(PROVIDER).unwrap() {
        Some(StoredCredential::OAuth(credential)) => {
            assert_eq!(credential.access, "fallback-access");
        }
        other => panic!("expected a stored OAuth credential, got {other:?}"),
    }

    service.unregister_auth_hooks(PROVIDER);
}

// ---------------------------------------------------------------------------
// Logout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn native_logout_runs_hook_then_clears_the_store() {
    const PROVIDER: &str = "native-logout-test";
    let (_dir, storage) = temp_storage("logout.json");
    storage
        .store(
            PROVIDER,
            StoredCredential::ApiKey(pir::ai::types::ApiKeyCredential::new(Some(
                "key".to_string(),
            ))),
        )
        .unwrap();

    let service = TestProviderService;
    let hooks = Arc::new(FakeHooks::default());
    service.register_auth_hooks(PROVIDER, hooks.clone());

    let existed = logout_stored(storage.clone(), PROVIDER)
        .await
        .expect("native logout succeeds");
    assert!(existed);
    assert_eq!(hooks.counts().2, 1);
    assert!(storage.read_stored(PROVIDER).unwrap().is_none());

    service.unregister_auth_hooks(PROVIDER);
}

// ---------------------------------------------------------------------------
// Precedence / registry lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn last_native_registration_wins_and_fallback_runs_once() {
    const PROVIDER: &str = "native-precedence-test";
    let (_dir, storage) = temp_storage("precedence.json");
    let service = TestProviderService;

    let first = Arc::new(FakeHooks::login_ok(ProviderAuth::api_key("first")));
    let second = Arc::new(FakeHooks::login_ok(ProviderAuth::api_key("second")));
    service.register_auth_hooks(PROVIDER, first.clone());
    service.register_auth_hooks(PROVIDER, second.clone());

    let fallback = CountingFlow::default();
    let interaction = TestInteraction;
    let options = LoginOptions::default();
    let auth = login_and_store(
        storage.as_ref(),
        PROVIDER,
        &fallback,
        &interaction,
        &options,
    )
    .await
    .expect("native login succeeds");

    assert_eq!(auth.api_key.as_deref(), Some("second"));
    assert_eq!(second.counts().0, 1);
    assert_eq!(first.counts().0, 0);
    // One flow total: the JS/builtin fallback is never consulted.
    assert_eq!(fallback.login_count(), 0);

    service.unregister_auth_hooks(PROVIDER);
}

#[tokio::test]
async fn unregister_removes_the_seam_and_restores_the_fallback() {
    const PROVIDER: &str = "native-unregister-test";
    let (_dir, storage) = temp_storage("unregister.json");
    let service = TestProviderService;

    let hooks = Arc::new(FakeHooks::login_ok(ProviderAuth::api_key("native")));
    service.register_auth_hooks(PROVIDER, hooks.clone());
    // The lookup is the typed service method, no downcast.
    assert!(service.auth_hooks(PROVIDER).is_some());
    // A hook-only provider discovers a login flow through the standard seam.
    assert!(oauth_flow_for(PROVIDER, None).is_some());

    service.unregister_auth_hooks(PROVIDER);
    assert!(service.auth_hooks(PROVIDER).is_none());
    assert!(oauth_flow_for(PROVIDER, None).is_none());

    let fallback = CountingFlow::default();
    let interaction = TestInteraction;
    let options = LoginOptions::default();
    let auth = login_and_store(
        storage.as_ref(),
        PROVIDER,
        &fallback,
        &interaction,
        &options,
    )
    .await
    .expect("fallback login succeeds");

    assert_eq!(auth.access_token.as_deref(), Some("fallback-access"));
    assert_eq!(fallback.login_count(), 1);
    assert_eq!(hooks.counts().0, 0);
    match storage.read_stored(PROVIDER).unwrap() {
        Some(StoredCredential::OAuth(credential)) => {
            assert_eq!(credential.access, "fallback-access");
        }
        other => panic!("expected a stored OAuth credential, got {other:?}"),
    }
}

/// `ProviderRegistry` exposes the same registration/lookup seam for providers
/// assembled outside a plugin context.
#[test]
fn provider_registry_exposes_the_auth_hook_seam() {
    const PROVIDER: &str = "native-registry-test";
    let registry = pir::ai::provider::ProviderRegistry::default();
    let hooks: Arc<dyn ProviderAuthHooks> =
        Arc::new(FakeHooks::login_ok(ProviderAuth::api_key("k")));

    registry.register_auth_hooks(PROVIDER, hooks);
    assert!(registry.auth_hooks(PROVIDER).is_some());
    assert!(registry.unregister_auth_hooks(PROVIDER));
    assert!(registry.auth_hooks(PROVIDER).is_none());
}

/// Keep the failure type's variants constructible from tests (compile-time
/// coverage of the typed error surface).
#[test]
fn provider_auth_error_variants_are_typed() {
    let errors = [
        ProviderAuthError::Failed("failed".to_string()),
        ProviderAuthError::Unavailable,
        ProviderAuthError::Cancelled,
    ];
    assert!(errors.iter().all(|error| !error.to_string().is_empty()));
    let _: AiError = AiError::Authentication(ProviderAuthError::Cancelled.to_string());
}
