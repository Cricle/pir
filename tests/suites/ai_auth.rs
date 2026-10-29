use pir::ai::auth::{
    ApiKeyAuth, AuthContext, CredentialStore, EnvAuthContext, InMemoryCredentialStore,
    MapAuthContext,
};
use pir::ai::error::{AiError, AiResult};
use pir::ai::types::{ApiKeyCredential, Credential, CredentialType};
use std::collections::BTreeMap;

fn store_with(key: &str) -> InMemoryCredentialStore {
    let store = InMemoryCredentialStore::new();
    let credential = Credential::ApiKey(ApiKeyCredential::new(Some(key.to_owned())));
    let mut f =
        |_: Option<&Credential>| -> AiResult<Option<Credential>> { Ok(Some(credential.clone())) };
    store.modify("faux", &mut f).unwrap();
    store
}

#[test]
fn auth_context_reads_values_and_ignores_missing() {
    let mut values = BTreeMap::new();
    values.insert("FOO_KEY".to_owned(), "secret-foo".to_owned());
    let ctx = MapAuthContext::new(values);
    assert_eq!(ctx.env("FOO_KEY").unwrap(), Some("secret-foo".to_owned()));
    assert_eq!(ctx.env("MISSING").unwrap(), None);
    assert_eq!(
        EnvAuthContext::new()
            .env("PIR_DEFINITELY_UNSET_VAR_12345")
            .unwrap(),
        None
    );
}

#[test]
fn stored_credential_wins_and_metadata_hides_secrets() {
    let store = store_with("stored-secret");
    let credential = store.read("faux").unwrap();
    let auth = ApiKeyAuth::new("Faux", vec!["FAUX_API_KEY".to_owned()]);
    let ctx = MapAuthContext::new(BTreeMap::new());
    let key = credential.as_ref().map(|c| {
        let Credential::ApiKey(k) = c;
        k
    });
    let result = auth.resolve(&ctx, key).unwrap().unwrap();
    assert_eq!(result.auth.api_key.as_deref(), Some("stored-secret"));
    let info = store.list().unwrap();
    assert_eq!(info[0].credential_type, CredentialType::ApiKey);
}

#[test]
fn ambient_key_and_modify_delete_work() {
    let auth = ApiKeyAuth::new("Faux", vec!["FAUX_API_KEY".to_owned()]);
    let mut values = BTreeMap::new();
    values.insert("FAUX_API_KEY".to_owned(), "env-secret".to_owned());
    let result = auth
        .resolve(&MapAuthContext::new(values), None)
        .unwrap()
        .unwrap();
    assert_eq!(result.auth.api_key.as_deref(), Some("env-secret"));
    let store = InMemoryCredentialStore::new();
    let mut f = |_: Option<&Credential>| -> AiResult<Option<Credential>> {
        Ok(Some(Credential::ApiKey(ApiKeyCredential::new(Some(
            "a".to_owned(),
        )))))
    };
    store.modify("p", &mut f).unwrap();
    assert!(store.read("p").unwrap().is_some());
    store.delete("p").unwrap();
    assert!(store.read("p").unwrap().is_none());
}

// ---------------------------------------------------------------------------
// auth.json api-key templates resolved at READ time (upstream
// auth-storage.ts:263-266,444-446)
// ---------------------------------------------------------------------------

fn file_store(tag: &str) -> pir::ai::auth::storage::FileAuthStorage {
    let dir = std::env::temp_dir().join(format!(
        "pir-auth-templates-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    pir::ai::auth::storage::FileAuthStorage::new(dir.join("auth.json"))
}

#[test]
fn auth_json_env_templates_resolve_at_read_time() {
    let storage = file_store("env");
    // A literal key round-trips unchanged.
    storage
        .store(
            "literal",
            pir::ai::auth::storage::StoredCredential::ApiKey(ApiKeyCredential::new(Some(
                "sk-literal".into(),
            ))),
        )
        .unwrap();
    let credential = storage.read_stored("literal").unwrap().unwrap();
    let pir::ai::auth::storage::StoredCredential::ApiKey(credential) = credential else {
        panic!("api key credential");
    };
    assert_eq!(credential.key.as_deref(), Some("sk-literal"));

    // An `$ENV` template resolves against the credential env, then the
    // process environment.
    storage
        .store(
            "templated",
            pir::ai::auth::storage::StoredCredential::ApiKey(ApiKeyCredential {
                credential_type: pir::ai::types::CredentialType::ApiKey,
                key: Some("prefix-$ACME_TEMPLATE_KEY".into()),
                env: Some(BTreeMap::from([(
                    "ACME_TEMPLATE_KEY".to_owned(),
                    "resolved".to_owned(),
                )])),
            }),
        )
        .unwrap();
    let credential = storage.read_stored("templated").unwrap().unwrap();
    let pir::ai::auth::storage::StoredCredential::ApiKey(credential) = credential else {
        panic!("api key credential");
    };
    assert_eq!(credential.key.as_deref(), Some("prefix-resolved"));

    // A missing variable yields a credential with no key (upstream resolves
    // the template to undefined).
    storage
        .store(
            "missing",
            pir::ai::auth::storage::StoredCredential::ApiKey(ApiKeyCredential {
                credential_type: pir::ai::types::CredentialType::ApiKey,
                key: Some("$PIR_DEFINITELY_UNSET_VAR_98765".into()),
                env: None,
            }),
        )
        .unwrap();
    let credential = storage.read_stored("missing").unwrap().unwrap();
    let pir::ai::auth::storage::StoredCredential::ApiKey(credential) = credential else {
        panic!("api key credential");
    };
    assert!(credential.key.is_none());

    // `!command` values are returned raw at read time (upstream read()).
    storage
        .store(
            "command",
            pir::ai::auth::storage::StoredCredential::ApiKey(ApiKeyCredential::new(Some(
                "!echo from-command".into(),
            ))),
        )
        .unwrap();
    let credential = storage.read_stored("command").unwrap().unwrap();
    let pir::ai::auth::storage::StoredCredential::ApiKey(credential) = credential else {
        panic!("api key credential");
    };
    assert_eq!(credential.key.as_deref(), Some("!echo from-command"));
}

// ---------------------------------------------------------------------------
// Anthropic workload-identity federation (upstream providers/anthropic.ts:49-70)
// ---------------------------------------------------------------------------

#[test]
fn anthropic_federation_resolves_last_with_env_propagation() {
    let auth = ApiKeyAuth::new("Anthropic API key", vec!["ANTHROPIC_API_KEY".to_owned()])
        .with_federation(pir::ai::auth::anthropic_workload_identity_federation);

    // Required trio present -> header-less auth carrying the ids in env.
    let mut values = BTreeMap::new();
    values.insert(
        "ANTHROPIC_FEDERATION_RULE_ID".to_owned(),
        "rule-1".to_owned(),
    );
    values.insert("ANTHROPIC_ORGANIZATION_ID".to_owned(), "org-1".to_owned());
    values.insert(
        "ANTHROPIC_IDENTITY_TOKEN_FILE".to_owned(),
        "/tmp/token".to_owned(),
    );
    values.insert("ANTHROPIC_WORKSPACE_ID".to_owned(), "ws-1".to_owned());
    let ctx = MapAuthContext::new(values);
    let result = auth.resolve(&ctx, None).unwrap().expect("federation");
    assert_eq!(
        result.source.as_deref(),
        Some("workload identity federation")
    );
    assert!(result.auth.api_key.is_none());
    assert!(result.auth.headers.is_none());
    let env = result.env.expect("federation env");
    assert_eq!(
        env.get("ANTHROPIC_FEDERATION_RULE_ID").map(String::as_str),
        Some("rule-1")
    );
    assert_eq!(
        env.get("ANTHROPIC_ORGANIZATION_ID").map(String::as_str),
        Some("org-1")
    );
    assert_eq!(
        env.get("ANTHROPIC_IDENTITY_TOKEN_FILE").map(String::as_str),
        Some("/tmp/token")
    );
    assert_eq!(
        env.get("ANTHROPIC_WORKSPACE_ID").map(String::as_str),
        Some("ws-1")
    );
    assert!(!env.contains_key("ANTHROPIC_SERVICE_ACCOUNT_ID"));

    // A missing required variable -> no federation result.
    let ctx = MapAuthContext::new(BTreeMap::from([(
        "ANTHROPIC_FEDERATION_RULE_ID".to_owned(),
        "rule-1".to_owned(),
    )]));
    assert!(auth.resolve(&ctx, None).unwrap().is_none());

    // An env key wins over federation (upstream: last in line).
    let mut values = BTreeMap::new();
    values.insert("ANTHROPIC_API_KEY".to_owned(), "sk-key".to_owned());
    values.insert(
        "ANTHROPIC_FEDERATION_RULE_ID".to_owned(),
        "rule-1".to_owned(),
    );
    values.insert("ANTHROPIC_ORGANIZATION_ID".to_owned(), "org-1".to_owned());
    values.insert(
        "ANTHROPIC_IDENTITY_TOKEN_FILE".to_owned(),
        "/tmp/token".to_owned(),
    );
    let ctx = MapAuthContext::new(values);
    let result = auth.resolve(&ctx, None).unwrap().expect("env key");
    assert_eq!(result.auth.api_key.as_deref(), Some("sk-key"));

    // A stored credential wins too.
    let stored = ApiKeyCredential::new(Some("sk-stored".into()));
    let result = auth
        .resolve(&MapAuthContext::new(BTreeMap::new()), Some(&stored))
        .unwrap()
        .expect("stored key");
    assert_eq!(result.auth.api_key.as_deref(), Some("sk-stored"));
}

// ---------------------------------------------------------------------------
// auth.json cross-process lock file: release after the write, stale takeover
// (D10 alignment: the Windows transient-EPERM retry rides the same loop)
// ---------------------------------------------------------------------------

#[test]
fn auth_lock_file_releases_after_write_and_takes_over_stale() {
    let storage = file_store("lock");
    // The lock path is `<auth_path>.lock` (see FileAuthStorage docs).
    let lock_path = std::path::PathBuf::from(format!("{}.lock", storage.path().display()));
    assert!(!lock_path.exists());

    // A normal write acquires and releases the cross-process lock.
    storage
        .store(
            "locked",
            pir::ai::auth::storage::StoredCredential::ApiKey(ApiKeyCredential::new(Some(
                "sk-locked".into(),
            ))),
        )
        .expect("store under lock");
    assert!(
        !lock_path.exists(),
        "the lock file must not survive the completed write"
    );

    // A stale lock file (older than the 30s takeover window) must not block a
    // writer: the acquire loop removes it and proceeds.
    std::fs::write(&lock_path, b"stale").expect("seed stale lock");
    let file = std::fs::File::options()
        .write(true)
        .open(&lock_path)
        .expect("open stale lock");
    file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(31))
        .expect("age the lock file");
    drop(file);
    storage
        .store(
            "after-stale",
            pir::ai::auth::storage::StoredCredential::ApiKey(ApiKeyCredential::new(Some(
                "sk-fresh".into(),
            ))),
        )
        .expect("store after stale takeover");
    assert!(
        !lock_path.exists(),
        "the taken-over lock must be released when the write completes"
    );
    assert!(storage
        .read_stored("after-stale")
        .expect("read back")
        .is_some());
}

// ---------------------------------------------------------------------------
// Request-time auth from auth.json (upstream `Models.getAuth` /
// `auth/resolve.ts`): the registry reads the stored credential — OAuth
// included — instead of only the api-key view, so a `/login` credential
// reaches the request path.
// ---------------------------------------------------------------------------

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use pir::ai::auth::storage::StoredCredential;
use pir::ai::auth::{
    AuthInteraction, FileAuthStorage, LoginOptions, OAuthCredential, OAuthFlow, OAuthFlowBox,
};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Model, ModelAuth, ModelCost, ModelCostRates,
    StopReason, StreamOptions, TerminalReason, TextContent,
};

const PROBE_PROVIDER: &str = "authprobe";
const PROBE_ENV_VAR: &str = "AUTHPROBE_API_KEY";

fn probe_model() -> Model {
    Model {
        id: "probe-1".to_string(),
        name: "probe-1".to_string(),
        api: "authprobe-fake".to_string(),
        provider: PROBE_PROVIDER.to_string(),
        base_url: "http://127.0.0.1:9".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

fn probe_storage(tag: &str) -> Arc<FileAuthStorage> {
    let dir = std::env::temp_dir().join(format!(
        "pir-registry-auth-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    Arc::new(FileAuthStorage::new(dir.join("auth.json")))
}

/// A scripted provider with one api-key auth (an env var) and an optional
/// OAuth flow; records the credential each request carried.
struct ProbeProvider {
    flow: Option<OAuthFlowBox>,
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

impl Provider for ProbeProvider {
    fn id(&self) -> &str {
        PROBE_PROVIDER
    }
    fn name(&self) -> &str {
        "Auth probe"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![probe_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new(
            "Auth probe key",
            vec![PROBE_ENV_VAR.to_owned()],
        )]
    }
    fn oauth_flow(&self) -> Option<OAuthFlowBox> {
        self.flow.clone()
    }
    fn stream(
        &self,
        model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(options.api_key.clone());
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "ok".to_string(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                timestamp: 1,
                ..Default::default()
            },
        ));
        stream
    }
}

/// A minimal OAuth flow: `refresh` can succeed with a canned credential or
/// fail, and `to_auth` maps the access token to an api key.
struct StaticFlow {
    refreshed: Option<OAuthCredential>,
    fail_refresh: bool,
}

impl OAuthFlow for StaticFlow {
    fn name(&self) -> &str {
        "static"
    }
    fn login<'a>(
        &'a self,
        _interaction: &'a dyn AuthInteraction,
        _options: &'a LoginOptions,
    ) -> Pin<Box<dyn Future<Output = pir::ai::error::AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async { Err(pir::ai::error::AiError::Authentication("unused".into())) })
    }
    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> Pin<Box<dyn Future<Output = pir::ai::error::AiResult<OAuthCredential>> + Send + 'a>> {
        Box::pin(async move {
            if self.fail_refresh {
                return Err(pir::ai::error::AiError::Authentication(
                    "refresh failed".into(),
                ));
            }
            Ok(self.refreshed.clone().unwrap_or_else(|| credential.clone()))
        })
    }
    fn to_auth(&self, credential: &OAuthCredential) -> pir::ai::error::AiResult<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: None,
        })
    }
}

fn probe_registry(
    storage: Arc<FileAuthStorage>,
    flow: Option<OAuthFlowBox>,
    env: BTreeMap<String, String>,
) -> (ProviderRegistry, Arc<Mutex<Vec<Option<String>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(MapAuthContext::new(env)),
    )
    .with_auth_storage(storage);
    registry.set_provider(Arc::new(ProbeProvider {
        flow,
        seen: Arc::clone(&seen),
    }));
    (registry, seen)
}

#[tokio::test]
async fn registry_reads_stored_api_key_from_auth_json() {
    let storage = probe_storage("stored-key");
    storage
        .store(
            PROBE_PROVIDER,
            StoredCredential::ApiKey(ApiKeyCredential::new(Some("sk-auth-json".into()))),
        )
        .expect("store credential");
    // Ambient env also has a key: the stored credential owns the provider.
    let (registry, _seen) = probe_registry(
        storage,
        None,
        BTreeMap::from([(PROBE_ENV_VAR.to_owned(), "sk-from-env".to_owned())]),
    );

    let auth = registry
        .get_auth(&probe_model())
        .await
        .expect("auth resolution")
        .expect("stored credential resolves");
    assert_eq!(auth.auth.api_key.as_deref(), Some("sk-auth-json"));
    assert_eq!(auth.source.as_deref(), Some("stored credential"));
}

#[tokio::test]
async fn registry_resolves_stored_oauth_through_provider_flow() {
    let storage = probe_storage("oauth");
    let expires = pir::ai::util::now_ms() as i64 + 60 * 60 * 1000;
    storage
        .store(
            PROBE_PROVIDER,
            StoredCredential::OAuth(OAuthCredential::new("access-1", "refresh-1", expires)),
        )
        .expect("store credential");
    let flow: OAuthFlowBox = Arc::new(StaticFlow {
        refreshed: None,
        fail_refresh: false,
    });
    // An env key must not win over the stored OAuth credential.
    let (registry, _seen) = probe_registry(
        storage,
        Some(flow),
        BTreeMap::from([(PROBE_ENV_VAR.to_owned(), "sk-from-env".to_owned())]),
    );

    let auth = registry
        .get_auth(&probe_model())
        .await
        .expect("auth resolution")
        .expect("OAuth credential resolves");
    assert_eq!(auth.auth.api_key.as_deref(), Some("access-1"));
    assert_eq!(auth.source.as_deref(), Some("OAuth"));
}

#[tokio::test]
async fn registry_refreshes_expired_stored_oauth_and_persists() {
    let storage = probe_storage("oauth-refresh");
    storage
        .store(
            PROBE_PROVIDER,
            StoredCredential::OAuth(OAuthCredential::new("expired", "refresh-1", 0)),
        )
        .expect("store credential");
    let refreshed_expires = pir::ai::util::now_ms() as i64 + 60 * 60 * 1000;
    let flow: OAuthFlowBox = Arc::new(StaticFlow {
        refreshed: Some(OAuthCredential::new(
            "access-2",
            "refresh-2",
            refreshed_expires,
        )),
        fail_refresh: false,
    });
    let (registry, _seen) = probe_registry(storage.clone(), Some(flow), BTreeMap::new());

    let auth = registry
        .get_auth(&probe_model())
        .await
        .expect("auth resolution")
        .expect("refreshed OAuth credential resolves");
    assert_eq!(auth.auth.api_key.as_deref(), Some("access-2"));

    // The rotation is persisted before the lock is released.
    match storage.read_stored(PROBE_PROVIDER).expect("read back") {
        Some(StoredCredential::OAuth(credential)) => {
            assert_eq!(credential.access, "access-2");
            assert_eq!(credential.refresh, "refresh-2");
        }
        other => panic!("expected a stored OAuth credential, got {other:?}"),
    }
}

#[tokio::test]
async fn stored_oauth_without_a_flow_has_no_env_fallback() {
    let storage = probe_storage("oauth-no-flow");
    let expires = pir::ai::util::now_ms() as i64 + 60 * 60 * 1000;
    storage
        .store(
            PROBE_PROVIDER,
            StoredCredential::OAuth(OAuthCredential::new("access-1", "refresh-1", expires)),
        )
        .expect("store credential");
    let (registry, _seen) = probe_registry(
        storage,
        None,
        BTreeMap::from([(PROBE_ENV_VAR.to_owned(), "sk-from-env".to_owned())]),
    );

    let auth = registry
        .get_auth(&probe_model())
        .await
        .expect("auth resolution");
    assert!(
        auth.is_none(),
        "a stored credential without a matching handler stays unconfigured (upstream resolve.ts)"
    );
}

#[tokio::test]
async fn explicit_api_key_override_bypasses_stored_oauth() {
    let storage = probe_storage("override");
    storage
        .store(
            PROBE_PROVIDER,
            StoredCredential::OAuth(OAuthCredential::new("expired", "refresh-1", 0)),
        )
        .expect("store credential");
    // The stored OAuth credential cannot refresh; an explicit request key must
    // still resolve (upstream resolves overrides before reading the store).
    let flow: OAuthFlowBox = Arc::new(StaticFlow {
        refreshed: None,
        fail_refresh: true,
    });
    let (registry, seen) = probe_registry(storage, Some(flow), BTreeMap::new());

    registry
        .complete(
            probe_model(),
            Context::default(),
            StreamOptions {
                api_key: Some("override-key".to_string()),
                ..StreamOptions::default()
            },
        )
        .await
        .expect("the explicit key resolves without touching the stored OAuth entry");

    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(seen, vec![Some("override-key".to_string())]);
}

/// OAuth expiry bookkeeping takes provider-controlled values (`expires_in`
/// feeds `expires`). The comparisons/additions must saturate instead of
/// overflowing: a huge stored expiry is simply "not expiring soon", and a huge
/// validity margin saturates to a positive verdict rather than panicking.
#[test]
fn oauth_expiry_comparisons_saturate_instead_of_overflowing() {
    use pir::ai::auth::{oauth_expires_soon, OAuthCredential};

    let far_future = OAuthCredential::new("access", "refresh", i64::MAX);
    assert!(!oauth_expires_soon(&far_future, 0));
    assert!(!oauth_expires_soon(&far_future, i64::MAX / 2));

    // The margin itself can be caller-supplied; saturating to i64::MAX still
    // yields a deterministic (refresh-now) verdict.
    assert!(oauth_expires_soon(&far_future, i64::MAX));

    let expired = OAuthCredential::new("access", "refresh", 0);
    assert!(oauth_expires_soon(&expired, 0));
}

/// Serve one canned HTTP response on an ephemeral loopback listener and
/// return its base URL. The connection is read once (request head) and the
/// response bytes are written verbatim.
async fn serve_oauth_response(response: Vec<u8>) -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind mock OAuth listener");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut buffer = [0u8; 4096];
        let _ = stream.read(&mut buffer).await;
        let _ = stream.write_all(&response).await;
        let _ = stream.shutdown().await;
    });
    format!("http://{addr}/token")
}

/// OAuth token responses are bounded: a `Content-Length` past the cap fails
/// before reading, and a chunked body crossing the cap fails mid-stream. Both
/// are typed [`AiError::ResponseTooLarge`], never an unbounded `text()`.
#[tokio::test]
async fn oauth_response_body_is_bounded() {
    let limit = pir::ai::auth::http::MAX_RESPONSE_BODY_BYTES;

    // Announced length over the cap.
    let url = serve_oauth_response(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2097152\r\nconnection: close\r\n\r\n"
            .to_vec(),
    )
    .await;
    let error = pir::ai::auth::http::post_json(&url, &serde_json::json!({"grant_type": "x"}))
        .await
        .err()
        .expect("an oversized Content-Length must fail");
    assert!(
        matches!(error, AiError::ResponseTooLarge { limit: actual } if actual == limit),
        "expected ResponseTooLarge, got {error:?}"
    );

    // Chunked body without a Content-Length: the streaming check must trip
    // once the accumulated bytes cross the cap.
    let chunk = "a".repeat(600_000);
    let framed = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n\
         {:x}\r\n{chunk}\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n",
        chunk.len(),
        chunk.len()
    );
    let url = serve_oauth_response(framed.into_bytes()).await;
    let error = pir::ai::auth::http::post_json(&url, &serde_json::json!({"grant_type": "x"}))
        .await
        .err()
        .expect("a chunked body past the cap must fail");
    assert!(
        matches!(error, AiError::ResponseTooLarge { limit: actual } if actual == limit),
        "expected ResponseTooLarge, got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Credential redaction (P1): Debug output and OAuth error bodies must never
// carry access/refresh tokens.
// ---------------------------------------------------------------------------

#[test]
fn credential_debug_redacts_secrets() {
    let api_key = ApiKeyCredential::new(Some("sk-super-secret-key".into()));
    let debug = format!("{api_key:?}");
    assert!(
        !debug.contains("sk-super-secret-key"),
        "ApiKeyCredential Debug leaked the key: {debug}"
    );
    assert!(debug.contains("[REDACTED]"), "{debug}");

    let oauth = pir::ai::auth::OAuthCredential::new("access-secret", "refresh-secret", 42);
    let debug = format!("{oauth:?}");
    assert!(
        !debug.contains("access-secret") && !debug.contains("refresh-secret"),
        "OAuthCredential Debug leaked a token: {debug}"
    );
    assert!(debug.contains("[REDACTED]"), "{debug}");

    let auth = pir::ai::types::ModelAuth {
        api_key: Some("sk-model-auth-secret".into()),
        headers: None,
        base_url: None,
    };
    let debug = format!("{auth:?}");
    assert!(
        !debug.contains("sk-model-auth-secret"),
        "ModelAuth Debug leaked the key: {debug}"
    );
    assert!(debug.contains("[REDACTED]"), "{debug}");
}

#[test]
fn stream_options_debug_redacts_api_key() {
    let options = pir::ai::types::StreamOptions {
        api_key: Some("sk-options-secret".into()),
        ..Default::default()
    };
    let debug = format!("{options:?}");
    assert!(
        !debug.contains("sk-options-secret"),
        "StreamOptions Debug leaked the api key: {debug}"
    );
    assert!(debug.contains("[REDACTED]"), "{debug}");
}

#[test]
fn oauth_http_failure_redacts_echoed_tokens() {
    let body = serde_json::json!({
        "error": "invalid_grant",
        "error_description": "the grant expired",
        "access_token": "at-secret",
        "refresh_token": "rt-secret",
        "nested": { "id_token": "id-secret" }
    });
    let error = pir::ai::auth::http::oauth_http_failure("token refresh", 400, &body);
    let text = error.to_string();
    assert!(text.contains("invalid_grant"), "{text}");
    for secret in ["at-secret", "rt-secret", "id-secret"] {
        assert!(
            !text.contains(secret),
            "OAuth failure text leaked {secret}: {text}"
        );
    }
}

#[test]
fn oauth_body_text_redaction_omits_non_json() {
    let text =
        pir::ai::auth::http::redact_oauth_body_text("<html>access_token=super-secret</html>");
    assert!(
        !text.contains("super-secret"),
        "non-JSON OAuth bodies must not be echoed: {text}"
    );
}
