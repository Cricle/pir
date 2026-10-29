//! Lane G: Models management API, deferred responses, ToolChoice, header
//! transforms/identity, timeoutMs, and fetch injection (audit §5.2 #4/#5/#8/
//! #15/#16/#17/#19/#20/#30).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use futures_util::StreamExt;
use pir::agent::tool_execution::BoxFuture;
use pir::ai::api::anthropic::build_anthropic_request;
use pir::ai::api::bedrock::build_bedrock_body;
use pir::ai::api::openai::OpenAiCompatibleProvider;
use pir::ai::auth::{
    ApiKeyAuth, AuthEvent, AuthInteraction, AuthPrompt, CredentialStore, FileAuthStorage,
    InMemoryCredentialStore, LoginOptions, OAuthCredential, OAuthFlow, StoredCredential,
};
use pir::ai::error::AiResult;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{
    ModelsPublication, ModelsRefreshOptions, Provider, ProviderFuture, ProviderRegistry,
    RefreshModelsContext,
};
use pir::ai::transport::{
    HttpBodyChunk, HttpRequest, HttpResponse, HttpTransport, ProviderRequestClient,
    ProviderRequestConfig,
};
use pir::ai::types::*;
use serde_json::json;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn model(id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "openai-chat".to_string(),
        provider: "mock".to_string(),
        base_url: "https://example.invalid/v1".to_string(),
        model_type: None,
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        input_limits: None,
        cost: pir::ai::types::ModelCost {
            rates: pir::ai::types::ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: vec![],
        },
        context_window: 8192,
        max_tokens: 1024,
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        headers: None,
        prompt_cache: None,
        thinking_token_budget_field: None,
        compat: None,
    }
}

fn done(message: AssistantMessage) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let producer = stream.producer();
    producer.push(AssistantMessageEvent::Start {
        partial: Arc::new(message.clone()),
    });
    producer.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        message,
    ));
    producer.end(None);
    stream
}

fn assistant(model: &Model) -> AssistantMessage {
    AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: StopReason::Stop,
        ..AssistantMessage::default()
    }
}

/// Provider whose terminal message starts inside its own stream (a fresh
/// wall-clock timestamp), used to pin the registry stream's `durationMs`
/// timing: `stream`/`stream_simple`/`complete`/`complete_simple` callers must
/// see the same field the agent loop publishes.
struct TimedProvider {
    model: Model,
    /// `None` stamps `now_ms()` inside `stream`; `Some` reuses a fixed value.
    timestamp: Option<u64>,
    /// A pre-set `durationMs` (idempotence: the stream must not overwrite it).
    duration_ms: Option<u64>,
    delay: Duration,
}

impl Provider for TimedProvider {
    fn id(&self) -> &str {
        "timed"
    }

    fn name(&self) -> &str {
        "Timed provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        key_auth()
    }

    fn stream(
        &self,
        model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        std::thread::sleep(self.delay);
        let mut message = assistant(model);
        message.timestamp = self.timestamp.unwrap_or_else(pir::ai::util::now_ms);
        message.duration_ms = self.duration_ms;
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn timed_registry(
    timestamp: Option<u64>,
    duration_ms: Option<u64>,
    delay: Duration,
) -> (ProviderRegistry, Model) {
    let mut registry = registry_with(
        store_with_key("timed", "secret"),
        Arc::new(pir::ai::auth::EnvAuthContext::new()),
    );
    let mut request_model = model("timed-model");
    request_model.provider = "timed".to_string();
    registry.set_provider(Arc::new(TimedProvider {
        model: request_model.clone(),
        timestamp,
        duration_ms,
        delay,
    }) as Arc<dyn Provider>);
    (registry, request_model)
}

fn empty_context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![],
        tools: None,
    }
}

// ---------------------------------------------------------------------------
// durationMs: direct registry callers get the same terminal shape as the loop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registry_stream_paths_stamp_duration_ms() {
    let (registry, request_model) = timed_registry(None, None, Duration::from_millis(5));

    // `stream`: both the terminal event and the final result carry it.
    let mut stream = registry.stream(
        request_model.clone(),
        empty_context(),
        StreamOptions::default(),
    );
    let mut event_duration = None;
    while let Some(event) = stream.next().await {
        if let AssistantMessageEvent::Done { message, .. } = event {
            event_duration = message.duration_ms;
        }
    }
    let event_duration = event_duration.expect("terminal event carries durationMs");
    assert!(
        event_duration >= 5,
        "durationMs covers the response: got {event_duration}"
    );
    let streamed = stream.result().await.expect("final result");
    assert_eq!(
        streamed.duration_ms,
        Some(event_duration),
        "the event and result() agree"
    );

    // `complete` is the drained stream: same terminal message shape.
    let completed = registry
        .complete(
            request_model.clone(),
            empty_context(),
            StreamOptions::default(),
        )
        .await
        .expect("complete");
    assert!(
        completed.duration_ms.is_some_and(|duration| duration >= 5),
        "complete carries durationMs: {:?}",
        completed.duration_ms
    );

    // The simple entry points go through the same stream.
    let simple = registry.stream_simple(
        request_model.clone(),
        empty_context(),
        StreamOptions::default(),
    );
    let simple_message = simple.drain_and_result().await.expect("stream_simple");
    assert!(
        simple_message
            .duration_ms
            .is_some_and(|duration| duration >= 5),
        "stream_simple carries durationMs: {:?}",
        simple_message.duration_ms
    );
    let simple_complete = registry
        .complete_simple(request_model, empty_context(), StreamOptions::default())
        .await
        .expect("complete_simple");
    assert!(
        simple_complete
            .duration_ms
            .is_some_and(|duration| duration >= 5),
        "complete_simple carries durationMs: {:?}",
        simple_complete.duration_ms
    );
}

#[tokio::test]
async fn registry_preserves_a_pre_stamped_duration_ms() {
    let (registry, request_model) = timed_registry(None, Some(1234), Duration::ZERO);
    let completed = registry
        .complete(
            request_model.clone(),
            empty_context(),
            StreamOptions::default(),
        )
        .await
        .expect("complete");
    assert_eq!(completed.duration_ms, Some(1234));
    let streamed = registry
        .stream(request_model, empty_context(), StreamOptions::default())
        .drain_and_result()
        .await
        .expect("stream");
    assert_eq!(streamed.duration_ms, Some(1234));
}

#[tokio::test]
async fn registry_leaves_messages_that_predate_the_stream_untimed() {
    // A deferred-style response: its timestamp (60s before the fetch) predates
    // the stream that forwards it, so the upstream rule leaves it untimed.
    let (registry, request_model) = timed_registry(
        Some(pir::ai::util::now_ms().saturating_sub(60_000)),
        None,
        Duration::ZERO,
    );
    let completed = registry
        .complete(request_model, empty_context(), StreamOptions::default())
        .await
        .expect("complete");
    assert_eq!(completed.duration_ms, None);

    // The default/missing timestamp boundary (0) is untimed as well.
    let (registry, request_model) = timed_registry(Some(0), None, Duration::ZERO);
    let streamed = registry
        .stream(request_model, empty_context(), StreamOptions::default())
        .drain_and_result()
        .await
        .expect("stream");
    assert_eq!(streamed.duration_ms, None);
}

/// Scripted provider used by the management/deferred/header tests.
struct MockProvider {
    id: String,
    models: Arc<RwLock<Vec<Model>>>,
    refresh: bool,
    refreshed: Arc<AtomicUsize>,
    published: Arc<AtomicBool>,
    captured: Arc<Mutex<Option<StreamOptions>>>,
    deferred_supported: bool,
    cancelled: Arc<AtomicUsize>,
    filter_hidden: bool,
    headers: Option<ProviderHeaders>,
    api_key_auth: Vec<ApiKeyAuth>,
    oauth: bool,
}

impl MockProvider {
    fn new(id: &str, models: Vec<Model>) -> Self {
        Self {
            id: id.to_string(),
            models: Arc::new(RwLock::new(models)),
            refresh: false,
            refreshed: Arc::new(AtomicUsize::new(0)),
            published: Arc::new(AtomicBool::new(false)),
            captured: Arc::new(Mutex::new(None)),
            deferred_supported: false,
            cancelled: Arc::new(AtomicUsize::new(0)),
            filter_hidden: false,
            headers: None,
            api_key_auth: vec![],
            oauth: false,
        }
    }
}

impl Provider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.id
    }

    fn get_models(&self) -> Vec<Model> {
        self.models
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn stream(
        &self,
        model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        *self
            .captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(options);
        done(assistant(model))
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        self.api_key_auth
            .iter()
            .map(|auth| {
                ApiKeyAuth::new(auth.name.clone(), auth.env_vars.clone())
                    .with_bearer_env_vars(auth.bearer_env_vars.clone())
            })
            .collect()
    }

    fn oauth_flow(&self) -> Option<pir::ai::auth::OAuthFlowBox> {
        self.oauth
            .then(|| Arc::new(TestOAuthFlow) as pir::ai::auth::OAuthFlowBox)
    }

    fn headers(&self) -> Option<ProviderHeaders> {
        self.headers.clone()
    }

    fn filter_models(
        &self,
        models: Vec<Model>,
        _credential: Option<&pir::ai::auth::StoredCredential>,
    ) -> Vec<Model> {
        if self.filter_hidden {
            models
                .into_iter()
                .filter(|model| !model.id.starts_with("hidden"))
                .collect()
        } else {
            models
        }
    }

    fn supports_refresh(&self) -> bool {
        self.refresh
    }

    fn refresh_models(
        &self,
        context: RefreshModelsContext,
    ) -> Option<ProviderFuture<AiResult<()>>> {
        let models = self.models.clone();
        let refreshed = self.refreshed.clone();
        let published = self.published.clone();
        Some(Box::pin(async move {
            refreshed.fetch_add(1, Ordering::SeqCst);
            if context.allow_network {
                models
                    .write()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push(model("refreshed-model"));
            }
            published.store(
                context
                    .publish(ModelsPublication {
                        persist: Some(Some(pir::ai::providers::models_store::ModelsStoreEntry {
                            models: vec![json!({"id": "refreshed-model"})],
                            ..Default::default()
                        })),
                        update: None,
                    })
                    .await,
                Ordering::SeqCst,
            );
            Ok(())
        }))
    }

    fn fetch_deferred(
        &self,
        model: &Model,
        handle: DeferredHandle,
        _options: StreamOptions,
    ) -> Option<AssistantMessageEventStream> {
        if !self.deferred_supported {
            return None;
        }
        let mut message = assistant(model);
        message.deferred = Some(handle);
        Some(done(message))
    }

    fn cancel_deferred(
        &self,
        _model: &Model,
        _handle: DeferredHandle,
        _options: StreamOptions,
    ) -> Option<ProviderFuture<AiResult<()>>> {
        if !self.deferred_supported {
            return None;
        }
        let cancelled = self.cancelled.clone();
        Some(Box::pin(async move {
            cancelled.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }))
    }
}

fn deferred_handle(model: &Model) -> DeferredHandle {
    DeferredHandle {
        provider: model.provider.clone(),
        model_id: model.id.clone(),
        api: model.api.clone(),
        id: "deferred-1".to_string(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    }
}

fn store_with_key(provider: &str, key: &str) -> Arc<InMemoryCredentialStore> {
    let store = Arc::new(InMemoryCredentialStore::new());
    store
        .modify(provider, &mut |_| {
            Ok(Some(Credential::ApiKey(ApiKeyCredential::new(Some(
                key.to_string(),
            )))))
        })
        .expect("store key");
    store
}

fn key_auth() -> Vec<ApiKeyAuth> {
    vec![ApiKeyAuth::new("Mock key", vec![])]
}

/// Login interaction that answers every prompt with a fixed api key.
struct TestInteraction;

impl AuthInteraction for TestInteraction {
    fn prompt<'a>(
        &'a self,
        _prompt: AuthPrompt,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<String>> + Send + 'a>> {
        Box::pin(async { Ok("entered-key".to_string()) })
    }

    fn notify(&self, _event: AuthEvent) {}
}

/// OAuth flow used by the login test.
struct TestOAuthFlow;

impl OAuthFlow for TestOAuthFlow {
    fn name(&self) -> &str {
        "Test OAuth"
    }

    fn login<'a>(
        &'a self,
        _interaction: &'a dyn AuthInteraction,
        _options: &'a LoginOptions,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<OAuthCredential>> + Send + 'a>>
    {
        Box::pin(async { Ok(OAuthCredential::new("access", "refresh", i64::MAX / 2)) })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<OAuthCredential>> + Send + 'a>>
    {
        Box::pin(async move { Ok(credential.clone()) })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        })
    }
}

fn boxed_provider(provider: MockProvider) -> Arc<dyn Provider> {
    Arc::new(provider)
}

fn registry_with(
    store: Arc<dyn CredentialStore>,
    auth: Arc<dyn pir::ai::auth::AuthContext>,
) -> ProviderRegistry {
    ProviderRegistry::new(store, auth)
}

// ---------------------------------------------------------------------------
// ToolChoice
// ---------------------------------------------------------------------------

#[test]
fn tool_choice_serde_roundtrip_and_wire_shapes() {
    for (choice, wire) in [
        (ToolChoice::Auto, json!("auto")),
        (ToolChoice::None, json!("none")),
        (ToolChoice::Any, json!("any")),
        (ToolChoice::Required, json!("required")),
        (
            ToolChoice::Tool {
                name: "read_file".to_string(),
            },
            json!({"type": "tool", "name": "read_file"}),
        ),
    ] {
        let serialized = serde_json::to_value(&choice).unwrap();
        assert_eq!(serialized, wire);
        let roundtrip: ToolChoice = serde_json::from_value(serialized).unwrap();
        assert_eq!(roundtrip, choice);
    }
    let chat: ToolChoice =
        serde_json::from_value(json!({"type": "function", "function": {"name": "t"}})).unwrap();
    assert_eq!(chat, ToolChoice::Tool { name: "t".into() });
    let responses: ToolChoice =
        serde_json::from_value(json!({"type": "function", "name": "t"})).unwrap();
    assert_eq!(responses, ToolChoice::Tool { name: "t".into() });

    assert_eq!(
        ToolChoice::Any.openai_chat_json(),
        json!("required"),
        "OpenAI chat spells any as required"
    );
    assert_eq!(
        ToolChoice::Tool { name: "t".into() }.openai_chat_json(),
        json!({"type": "function", "function": {"name": "t"}})
    );
    assert_eq!(
        ToolChoice::Tool { name: "t".into() }.openai_responses_json(),
        json!({"type": "function", "name": "t"})
    );
    assert_eq!(ToolChoice::Any.anthropic_json(), json!({"type": "any"}));
    assert_eq!(
        ToolChoice::Tool { name: "t".into() }.anthropic_json(),
        json!({"type": "tool", "name": "t"})
    );
    assert_eq!(
        ToolChoice::Tool { name: "t".into() }.bedrock_json(),
        json!({"tool": {"name": "t"}})
    );
    assert_eq!(ToolChoice::Any.google_mode(), "ANY");
}

fn one_tool() -> Option<Vec<Tool>> {
    Some(vec![Tool {
        name: "read_file".to_string(),
        description: "read".to_string(),
        parameters: json!({"type": "object"}),
        constrained_sampling: None,
    }])
}

#[test]
fn anthropic_tool_choice_any_and_named_are_emitted() {
    let model = model("claude-test");
    let context = Context {
        system_prompt: None,
        messages: vec![],
        tools: one_tool(),
    };
    let options = StreamOptions {
        tool_choice: Some(ToolChoice::Any),
        ..Default::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).unwrap();
    assert_eq!(body["tool_choice"], json!({"type": "any"}));

    let options = StreamOptions {
        tool_choice: Some(ToolChoice::Tool {
            name: "read_file".to_string(),
        }),
        ..Default::default()
    };
    let (body, _) = build_anthropic_request(&model, &context, &options).unwrap();
    assert_eq!(
        body["tool_choice"],
        json!({"type": "tool", "name": "read_file"})
    );
}

#[test]
fn bedrock_tool_choice_maps_to_converse_shape() {
    let model = model("bedrock-test");
    let context = Context {
        system_prompt: None,
        messages: vec![],
        tools: one_tool(),
    };
    let options = StreamOptions {
        tool_choice: Some(ToolChoice::Any),
        ..Default::default()
    };
    let body = build_bedrock_body(&model, &context, &options).unwrap();
    assert_eq!(body["toolConfig"]["toolChoice"], json!({"any": {}}));

    let options = StreamOptions {
        tool_choice: Some(ToolChoice::Tool {
            name: "read_file".to_string(),
        }),
        ..Default::default()
    };
    let body = build_bedrock_body(&model, &context, &options).unwrap();
    assert_eq!(
        body["toolConfig"]["toolChoice"],
        json!({"tool": {"name": "read_file"}})
    );

    let options = StreamOptions {
        tool_choice: Some(ToolChoice::None),
        ..Default::default()
    };
    let body = build_bedrock_body(&model, &context, &options).unwrap();
    assert!(
        body.get("toolConfig").is_none(),
        "none drops the whole toolConfig"
    );
}

// ---------------------------------------------------------------------------
// Models management API
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_all_models_and_available_filter_by_auth_and_policy() {
    let (mut registry, mock_provider) = {
        let registry = registry_with(
            store_with_key("mock", "secret"),
            Arc::new(pir::ai::auth::EnvAuthContext::new()),
        );
        let provider = MockProvider {
            models: Arc::new(RwLock::new(vec![model("visible"), model("hidden-model")])),
            filter_hidden: true,
            api_key_auth: key_auth(),
            ..MockProvider::new("mock", vec![])
        };
        (registry, provider)
    };
    registry.set_provider(boxed_provider(mock_provider));

    assert_eq!(registry.get_models().len(), 2);
    assert_eq!(registry.get_all_models(None).len(), 2);
    assert_eq!(registry.get_all_models(Some("mock")).len(), 2);
    assert!(registry.get_all_models(Some("unknown")).is_empty());

    let check = registry
        .check_auth("mock")
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(check.auth_type, AuthType::ApiKey);
    assert!(registry.check_auth("unknown").await.unwrap().is_none());

    let available = registry.get_available(None).await.unwrap();
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].id, "visible");
    assert_eq!(
        registry
            .get_all_available(Some("mock"))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn get_available_requires_auth() {
    let mut registry = ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(pir::ai::auth::MapAuthContext::new(BTreeMap::new())),
    );
    registry.set_provider(boxed_provider(MockProvider {
        models: Arc::new(RwLock::new(vec![model("m")])),
        api_key_auth: key_auth(),
        ..MockProvider::new("mock", vec![])
    }));
    assert!(registry.check_auth("mock").await.unwrap().is_none());
    assert!(registry.get_available(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn logout_removes_stored_credential() {
    let store = store_with_key("mock", "secret");
    let mut registry = registry_with(
        store.clone(),
        Arc::new(pir::ai::auth::EnvAuthContext::new()),
    );
    registry.set_provider(boxed_provider(MockProvider {
        models: Arc::new(RwLock::new(vec![model("m")])),
        api_key_auth: key_auth(),
        ..MockProvider::new("mock", vec![])
    }));
    assert!(registry.check_auth("mock").await.unwrap().is_some());
    registry.logout("mock").await.unwrap();
    assert!(store.read("mock").unwrap().is_none());
    assert!(registry.check_auth("mock").await.unwrap().is_none());
}

#[tokio::test]
async fn login_persists_api_key_and_oauth_credentials() {
    // Api-key login stores into the injected credential store when no file
    // store is attached.
    let store = Arc::new(InMemoryCredentialStore::new());
    let mut registry = ProviderRegistry::new(
        store.clone(),
        Arc::new(pir::ai::auth::EnvAuthContext::new()),
    );
    registry.set_provider(boxed_provider(MockProvider {
        api_key_auth: key_auth(),
        ..MockProvider::new("mock", vec![model("m")])
    }));
    let auth = registry
        .login(
            "mock",
            AuthType::ApiKey,
            &TestInteraction,
            &LoginOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(auth.api_key.as_deref(), Some("entered-key"));
    assert!(store.read("mock").unwrap().is_some());
    assert!(registry.check_auth("mock").await.unwrap().is_some());

    // OAuth login requires the app file store and persists the credential
    // there; checkAuth reports the OAuth type without refreshing.
    let path = std::env::temp_dir().join(format!(
        "pir-lane-g-auth-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let storage = Arc::new(FileAuthStorage::new(path.clone()));
    let mut registry = ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(pir::ai::auth::EnvAuthContext::new()),
    )
    .with_auth_storage(storage.clone());
    registry.set_provider(boxed_provider(MockProvider {
        oauth: true,
        ..MockProvider::new("mock", vec![model("m")])
    }));
    let auth = registry
        .login(
            "mock",
            AuthType::OAuth,
            &TestInteraction,
            &LoginOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(auth.access_token.as_deref(), Some("access"));
    let check = registry
        .check_auth("mock")
        .await
        .unwrap()
        .expect("configured");
    assert_eq!(check.auth_type, AuthType::OAuth);
    assert!(matches!(
        storage.read_stored("mock").unwrap(),
        Some(StoredCredential::OAuth(_))
    ));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn copilot_filter_models_uses_available_model_ids() {
    use pir::ai::providers::construction::CatalogProviderEntry;

    let models = vec![model("allowed"), model("blocked")];
    let provider = CatalogProviderEntry::from_adapters(
        "github-copilot",
        "GitHub Copilot",
        BTreeMap::new(),
        models,
    );
    let credential = StoredCredential::OAuth(OAuthCredential {
        access: "access".to_string(),
        refresh: "refresh".to_string(),
        expires: i64::MAX / 2,
        extra: BTreeMap::from([("availableModelIds".to_string(), json!(["allowed"]))]),
    });
    let filtered = provider.filter_models(provider.get_models(), Some(&credential));
    assert_eq!(
        filtered
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["allowed"]
    );
    // Without the policy field every model stays available.
    let credential = StoredCredential::OAuth(OAuthCredential::new("a", "r", i64::MAX / 2));
    assert_eq!(
        provider
            .filter_models(provider.get_models(), Some(&credential))
            .len(),
        2
    );
}

#[tokio::test]
async fn refresh_runs_both_phases_and_publishes_catalog() {
    let mut env = BTreeMap::new();
    env.insert("MOCK_API_KEY".to_string(), "secret".to_string());
    let mut registry = ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(pir::ai::auth::MapAuthContext::new(env)),
    )
    .with_models_store(Arc::new(
        pir::ai::providers::models_store::InMemoryModelsStore::new(),
    ));
    let mock = MockProvider {
        refresh: true,
        api_key_auth: vec![ApiKeyAuth::new("Mock key", vec!["MOCK_API_KEY".into()])],
        ..MockProvider::new("mock", vec![model("base")])
    };
    let refreshed = mock.refreshed.clone();
    let published = mock.published.clone();
    let models = mock.models.clone();
    registry.set_provider(boxed_provider(mock));

    let result = registry.refresh(ModelsRefreshOptions::default()).await;
    assert!(!result.aborted);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(refreshed.load(Ordering::SeqCst), 2, "both phases ran");
    assert!(published.load(Ordering::SeqCst));
    assert_eq!(models.read().unwrap().len(), 2);
    let stored = registry
        .models_store()
        .read("mock")
        .unwrap()
        .expect("catalog persisted");
    assert_eq!(stored.models.len(), 1);
}

// ---------------------------------------------------------------------------
// Deferred responses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deferred_stream_fetch_and_cancel() {
    let mut registry = registry_with(
        store_with_key("mock", "secret"),
        Arc::new(pir::ai::auth::EnvAuthContext::new()),
    );
    let mock = MockProvider {
        deferred_supported: true,
        api_key_auth: key_auth(),
        ..MockProvider::new("mock", vec![model("m")])
    };
    let cancelled = mock.cancelled.clone();
    registry.set_provider(boxed_provider(mock));
    let request_model = model("m");
    let handle = deferred_handle(&request_model);

    let stream = registry.stream_deferred(
        request_model.clone(),
        handle.clone(),
        StreamOptions::default(),
    );
    let message = stream.drain_and_result().await.expect("terminal message");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(
        message.deferred.as_ref().map(|handle| handle.id.as_str()),
        Some("deferred-1")
    );

    let message = registry
        .fetch_deferred(
            request_model.clone(),
            handle.clone(),
            StreamOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Stop);

    registry
        .cancel_deferred(request_model, handle, StreamOptions::default())
        .await
        .unwrap();
    assert_eq!(cancelled.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn deferred_unsupported_provider_yields_error() {
    let mut registry = registry_with(
        store_with_key("mock", "secret"),
        Arc::new(pir::ai::auth::EnvAuthContext::new()),
    );
    registry.set_provider(boxed_provider(MockProvider {
        api_key_auth: key_auth(),
        ..MockProvider::new("mock", vec![model("m")])
    }));
    let request_model = model("m");
    let error = registry
        .cancel_deferred(
            request_model.clone(),
            deferred_handle(&request_model),
            StreamOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(format!("{error}").contains("does not support deferred responses"));

    let message = registry
        .fetch_deferred(
            request_model.clone(),
            deferred_handle(&request_model),
            StreamOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(message
        .error_message
        .as_deref()
        .unwrap_or_default()
        .contains("does not support deferred responses"));
}

// ---------------------------------------------------------------------------
// Headers: identity, static provider/model headers, null suppression,
// transformHeaders
// ---------------------------------------------------------------------------

#[tokio::test]
async fn identity_static_headers_and_transform_are_applied() {
    let mut registry = registry_with(
        store_with_key("openrouter", "secret"),
        Arc::new(pir::ai::auth::EnvAuthContext::new()),
    );
    registry = registry.with_identity(ClientIdentity::with_pi_attribution());
    let mock = MockProvider {
        api_key_auth: key_auth(),
        headers: Some(ProviderHeaders::from([(
            "x-provider-static".to_string(),
            "p".to_string(),
        )])),
        ..MockProvider::new("openrouter", vec![model("m")])
    };
    let captured = mock.captured.clone();
    registry.set_provider(boxed_provider(mock));

    let mut request_model = model("m");
    request_model.provider = "openrouter".to_string();
    request_model.base_url = "https://openrouter.ai/api/v1".to_string();
    request_model.headers = Some(ProviderHeaders::from([
        ("x-model-static".to_string(), "m".to_string()),
        ("x-remove-me".to_string(), "".to_string()),
    ]));
    let transform = HeaderTransformHook::new(Arc::new(|mut headers: ProviderHeaders| {
        Box::pin(async move {
            headers.insert("x-transformed".to_string(), "yes".to_string());
            Ok(headers)
        })
    }));
    let options = StreamOptions {
        identity: Some(ClientIdentity {
            user_agent: Some(UserAgentMode::Short),
            ..Default::default()
        }),
        transform_headers: transform,
        ..Default::default()
    };
    registry
        .stream(request_model, Context::default(), options)
        .drain_and_result()
        .await
        .expect("done");
    let captured = captured
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
        .expect("stream options captured");

    let headers = captured.headers.expect("headers assembled");
    assert_eq!(
        pir::ai::types::header_value(Some(&headers), "user-agent"),
        Some("pi")
    );
    assert_eq!(
        pir::ai::types::header_value(Some(&headers), "x-provider-static"),
        Some("p")
    );
    assert_eq!(
        pir::ai::types::header_value(Some(&headers), "x-model-static"),
        Some("m")
    );
    assert_eq!(
        pir::ai::types::header_value(Some(&headers), "x-transformed"),
        Some("yes")
    );
    assert_eq!(
        pir::ai::types::header_value(Some(&headers), "x-remove-me"),
        Some(""),
        "empty means suppressed"
    );
    let finalized = pir::ai::types::finalize_headers(headers);
    assert!(
        !finalized.contains_key("x-remove-me"),
        "suppressed headers are dropped on the wire"
    );
    assert_eq!(
        pir::ai::types::header_value(Some(&finalized), "HTTP-Referer"),
        Some("https://pi.dev")
    );
}

// ---------------------------------------------------------------------------
// Request-level Bedrock / Azure configuration
// ---------------------------------------------------------------------------

#[test]
fn bedrock_request_options_override_env_and_carry_metadata() {
    let model = model("bedrock-test");
    let context = Context {
        system_prompt: None,
        messages: vec![],
        tools: None,
    };
    let mut metadata = BTreeMap::new();
    metadata.insert("team".to_string(), "ai".to_string());
    let bedrock = BedrockRequestOptions {
        region: Some("eu-central-1".to_string()),
        profile: Some("work".to_string()),
        request_metadata: Some(metadata),
        bearer_token: Some("token".to_string()),
    };
    let options = StreamOptions {
        bedrock: Some(bedrock.clone()),
        ..Default::default()
    };
    let body = build_bedrock_body(&model, &context, &options).unwrap();
    assert_eq!(body["requestMetadata"]["team"], "ai");

    let mut env = BTreeMap::new();
    env.insert("AWS_REGION".to_string(), "us-west-2".to_string());
    assert_eq!(
        pir::ai::api::bedrock::resolve_bedrock_region_with_options(
            &model,
            Some(&env),
            Some(&bedrock)
        ),
        "eu-central-1",
        "the request-level region wins over env"
    );
    assert_eq!(
        pir::ai::api::bedrock::resolve_bedrock_region(&model, Some(&env)),
        "us-west-2"
    );
}

#[test]
fn azure_request_options_override_env() {
    use pir::ai::api::openai_responses::azure::{
        azure_responses_url_with_options, resolve_azure_base_url_with_options,
        resolve_deployment_name_with_options,
    };

    let model = model("gpt-4o");
    let azure = AzureRequestOptions {
        api_version: Some("2025-01-01".to_string()),
        resource_name: None,
        base_url: Some("https://resource.openai.azure.com".to_string()),
        deployment_name: Some("my-deployment".to_string()),
    };
    let url = azure_responses_url_with_options(&model, None, Some(&azure)).unwrap();
    assert_eq!(
        url,
        "https://resource.openai.azure.com/openai/v1/deployments/my-deployment/responses?api-version=2025-01-01"
    );
    assert_eq!(
        resolve_deployment_name_with_options(&model, None, Some(&azure)),
        "my-deployment"
    );
    assert_eq!(
        resolve_azure_base_url_with_options(&model, None, Some(&azure)).unwrap(),
        "https://resource.openai.azure.com/openai/v1"
    );
    // Without the overrides the historical env-driven behaviour is unchanged.
    let mut env = BTreeMap::new();
    env.insert(
        "AZURE_OPENAI_RESOURCE_NAME".to_string(),
        "env-resource".to_string(),
    );
    assert_eq!(
        resolve_azure_base_url_with_options(&model, Some(&env), None).unwrap(),
        "https://env-resource.openai.azure.com/openai/v1"
    );
}

#[test]
fn catalog_static_provider_headers_are_attached() {
    let copilot = pir::ai::providers::catalog::load_provider("github-copilot")
        .unwrap()
        .expect("copilot catalog");
    let headers = copilot.models[0]
        .headers
        .as_ref()
        .expect("copilot static headers");
    assert_eq!(headers["Copilot-Integration-Id"], "vscode-chat");
    assert_eq!(headers["Editor-Version"], "vscode/1.107.0");
    assert_eq!(headers["User-Agent"], "GitHubCopilotChat/0.35.0");

    let nvidia = pir::ai::providers::catalog::load_provider("nvidia")
        .unwrap()
        .expect("nvidia catalog");
    assert_eq!(
        nvidia.models[0].headers.as_ref().expect("nvidia headers")["NVCF-POLL-SECONDS"],
        "3600"
    );
}

// ---------------------------------------------------------------------------
// models.json null header suppression
// ---------------------------------------------------------------------------

#[test]
fn models_json_null_headers_become_suppression_markers() {
    let config = pir::ai::providers::parse_pi_config(
        r#"{
            "providers": {
                "acme": {
                    "headers": {"authorization": null, "x-provider": "p"},
                    "models": [
                        {"id": "m", "headers": {"x-api-key": null, "x-model": "m"}}
                    ]
                }
            }
        }"#,
    )
    .expect("models.json with null header values parses");
    let provider = &config.providers["acme"];
    let provider_headers = provider.headers.as_ref().expect("provider headers");
    assert_eq!(provider_headers["authorization"], "");
    assert_eq!(provider_headers["x-provider"], "p");
    let model_headers = provider.models[0].headers.as_ref().expect("model headers");
    assert_eq!(model_headers["x-api-key"], "");
    assert_eq!(model_headers["x-model"], "m");
}

// ---------------------------------------------------------------------------
// timeoutMs and fetch injection
// ---------------------------------------------------------------------------

struct StallingTransport;

impl HttpTransport for StallingTransport {
    fn execute(&self, _request: HttpRequest) -> BoxFuture<'_, AiResult<HttpResponse>> {
        Box::pin(async move {
            std::future::pending::<()>().await;
            unreachable!()
        })
    }
}

fn transport_config(max_retries: u32) -> ProviderRequestConfig {
    ProviderRequestConfig {
        timeout: Duration::from_secs(5),
        max_response_bytes: 1024 * 1024,
        max_retries,
        max_retry_delay: Duration::from_millis(1),
        proxy_url: None,
        http_idle_timeout: None,
    }
}

#[tokio::test]
async fn request_timeout_ms_is_enforced_by_the_transport() {
    let mut client =
        ProviderRequestClient::with_transport(transport_config(0), Arc::new(StallingTransport));
    client.set_request_timeout(Some(Duration::from_millis(20)));
    let mut chunks = Vec::new();
    let error = client
        .post_json_streaming(
            "https://example.invalid/v1/chat/completions",
            &BTreeMap::new(),
            &json!({"x": 1}),
            None,
            &ProviderStreamHooks::default(),
            &mut |chunk: &[u8]| {
                chunks.extend_from_slice(chunk);
                Ok(())
            },
        )
        .await
        .unwrap_err();
    let message = format!("{error}");
    assert!(
        message.contains("request timed out") && message.contains("timeoutMs"),
        "{message}"
    );
    assert!(chunks.is_empty());
}

/// Scripted transport that returns one SSE response regardless of the request
/// URL (the fetch-injection seam).
struct ScriptedSseTransport {
    body: Vec<u8>,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl HttpTransport for ScriptedSseTransport {
    fn execute(&self, request: HttpRequest) -> BoxFuture<'_, AiResult<HttpResponse>> {
        self.requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(request);
        let body = self.body.clone();
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: reqwest::header::HeaderMap::new(),
                body: Box::pin(futures_util::stream::iter(vec![Ok(HttpBodyChunk::new(
                    body,
                ))])),
            })
        })
    }
}

#[tokio::test]
async fn per_request_fetch_injection_drives_the_adapter() {
    let mut provider_model = model("m");
    provider_model.provider = "mock".to_string();
    let adapter =
        OpenAiCompatibleProvider::new("mock", "http://127.0.0.1:1", vec![provider_model.clone()])
            .expect("provider");
    let mut registry = ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(pir::ai::auth::EnvAuthContext::new()),
    );
    registry.set_provider(Arc::new(adapter));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let transport = Arc::new(ScriptedSseTransport {
        body: concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        )
        .as_bytes()
        .to_vec(),
        requests: requests.clone(),
    });
    let options = StreamOptions {
        api_key: Some("test-key".to_string()),
        fetch: Some(FetchTransport(transport)),
        max_retries: Some(0),
        ..Default::default()
    };
    let message = registry
        .stream_resolving_auth(provider_model, Context::default(), options)
        .drain_and_result()
        .await
        .expect("terminal message");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "the injected transport served the request"
    );
}

#[tokio::test]
async fn adapter_timeout_ms_bounds_the_request() {
    let mut provider_model = model("m");
    provider_model.provider = "mock".to_string();
    let adapter =
        OpenAiCompatibleProvider::new("mock", "http://127.0.0.1:1", vec![provider_model.clone()])
            .expect("provider");
    let mut registry = ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(pir::ai::auth::EnvAuthContext::new()),
    );
    registry.set_provider(Arc::new(adapter));
    let options = StreamOptions {
        api_key: Some("test-key".to_string()),
        fetch: Some(FetchTransport(Arc::new(StallingTransport))),
        timeout_ms: Some(20),
        max_retries: Some(0),
        ..Default::default()
    };
    let message = registry
        .stream_resolving_auth(provider_model, Context::default(), options)
        .drain_and_result()
        .await
        .expect("terminal message");
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("request timed out"),
        "{:?}",
        message.error_message
    );
}

// ---------------------------------------------------------------------------
// Images / classifier request budget (audit §5.1 #3): timeoutMs is the
// per-attempt TOTAL limit, while the transport config timeout keeps its
// historical connect-timeout meaning.
// ---------------------------------------------------------------------------

/// Accept exactly one connection and never write a response: the client must
/// give up on its own per-request deadline (the old behavior left it waiting
/// for the 5-minute HTTP idle timeout).
fn serve_hanging_response_once() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind hanging server");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0u8; 1024];
            while matches!(stream.read(&mut buffer), Ok(read) if read > 0) {}
        }
    });
    format!("http://{addr}")
}

fn image_model_at(base_url: &str) -> ImageModel {
    ImageModel {
        id: "openrouter/image-gen".into(),
        name: "Image gen".into(),
        api: ImageApi::OpenRouterImages,
        provider: "openrouter".into(),
        base_url: base_url.into(),
        input: vec![InputModality::Text],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: vec![],
        },
        headers: None,
        output: vec![OutputModality::Image],
    }
}

fn decisions_model_at(base_url: &str) -> ClassifierModel {
    ClassifierModel {
        id: "gpt-decisions".into(),
        name: "Decisions".into(),
        api: ClassifierApi::OpenAiDecisions,
        provider: "openai".into(),
        base_url: base_url.into(),
        input: vec![InputModality::Text],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: vec![],
        },
        headers: None,
        context_window: 128_000,
    }
}

fn bool_classifier_context() -> ClassifierContext {
    ClassifierContext {
        state: json!({"text": "judge this"}),
        images: vec![],
        questions: BTreeMap::from([(
            "approved".to_string(),
            ClassifierQuestion::Bool {
                instructions: "Approve?".into(),
                criteria: ClassifierBoolCriteria {
                    true_meaning: "approved".into(),
                    false_meaning: "rejected".into(),
                },
            },
        )]),
    }
}

#[tokio::test]
async fn image_and_classifier_request_client_sets_the_request_deadline() {
    let mut client = pir::ai::images::request_client(Some(20), Some(0), None).expect("client");
    assert_eq!(
        client.config.timeout,
        Duration::from_millis(20),
        "timeoutMs keeps the historical connect-timeout meaning"
    );
    // Mock transport: a server that accepted the request but never answers.
    client.set_transport(Arc::new(StallingTransport));
    let error = client
        .post_json_streaming(
            "https://example.invalid/v1/chat/completions",
            &BTreeMap::new(),
            &json!({"x": 1}),
            None,
            &ProviderStreamHooks::default(),
            &mut |_chunk: &[u8]| Ok(()),
        )
        .await
        .unwrap_err();
    let message = format!("{error}");
    assert!(
        message.contains("request timed out") && message.contains("(timeoutMs)"),
        "{message}"
    );
}

#[tokio::test]
async fn image_generation_timeout_ms_bounds_a_hanging_server() {
    let base_url = serve_hanging_response_once();
    let started = std::time::Instant::now();
    let result = pir::ai::images::generate_images_with(
        &image_model_at(&base_url),
        &ImagesContext::from_prompt("draw a cat"),
        &ImagesRequestOptions {
            api_key: Some("test-key".into()),
            timeout_ms: Some(50),
            max_retries: Some(0),
            ..ImagesRequestOptions::default()
        },
    )
    .await;
    assert_eq!(result.stop_reason, ImagesStopReason::Error);
    let message = result.error_message.unwrap_or_default();
    assert!(message.contains("request timed out"), "{message}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the request deadline, not the idle timeout, must end the wait"
    );
}

#[tokio::test]
async fn decisions_classifier_timeout_ms_bounds_a_hanging_server() {
    let base_url = serve_hanging_response_once();
    let started = std::time::Instant::now();
    let result = pir::ai::classify::classify_with(
        &decisions_model_at(&base_url),
        &bool_classifier_context(),
        &ClassifierRequestOptions {
            api_key: Some("test-key".into()),
            timeout_ms: Some(50),
            max_retries: Some(0),
            ..ClassifierRequestOptions::default()
        },
    )
    .await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    let message = result.error_message.unwrap_or_default();
    assert!(message.contains("request timed out"), "{message}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the request deadline, not the idle timeout, must end the wait"
    );
}

// ---------------------------------------------------------------------------
// Radius dynamic catalog (upstream `providers/radius.ts` +
// `providers/radius-config.ts`)
// ---------------------------------------------------------------------------

use pir::ai::providers::models_store::ModelsStoreEntry;

/// Serve one JSON response over loopback and capture the request bytes.
fn serve_json_once(body: &str) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind localhost");
    let addr = listener.local_addr().expect("local addr");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept one connection");
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = stream.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if String::from_utf8_lossy(&request).contains("\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(response.as_bytes())
            .expect("write mock response");
        String::from_utf8_lossy(&request).to_string()
    });
    (format!("http://{addr}"), handle)
}

fn radius_refresh_context(
    stored: Option<ModelsStoreEntry>,
    allow_network: bool,
    credential: Option<StoredCredential>,
    log: Arc<Mutex<(usize, Option<ModelsStoreEntry>)>>,
) -> RefreshModelsContext {
    let publisher: pir::ai::provider::ModelsPublisher = Arc::new(
        move |publication: ModelsPublication| -> ProviderFuture<bool> {
            let mut captured = log.lock().unwrap_or_else(|poison| poison.into_inner());
            captured.0 += 1;
            if let Some(Some(entry)) = publication.persist.clone() {
                captured.1 = Some(entry);
            }
            drop(captured);
            if let Some(update) = publication.update.as_ref() {
                update();
            }
            Box::pin(async { true })
        },
    );
    RefreshModelsContext {
        credential,
        stored,
        publisher,
        allow_network,
        force: false,
        signal: AbortSignal::new(),
        base_url: None,
    }
}

#[test]
fn radius_gateway_url_normalization_matches_upstream() {
    use pir::ai::providers::radius::normalize_gateway_url;
    assert_eq!(
        normalize_gateway_url("gw.example.com"),
        "https://gw.example.com"
    );
    assert_eq!(
        normalize_gateway_url("http://gw.example.com///"),
        "http://gw.example.com"
    );
    assert_eq!(
        normalize_gateway_url("https://gw.example.com/path/"),
        "https://gw.example.com/path"
    );
}

#[tokio::test]
async fn radius_restores_the_persisted_catalog_without_network() {
    let mut stored_model = model("stored-model");
    stored_model.provider = "radius".to_string();
    let provider = pir::ai::providers::radius::RadiusProvider::new(
        "radius",
        "Radius",
        vec![model("baseline-model")],
        None,
    )
    .expect("radius provider");
    let log = Arc::new(Mutex::new((0usize, None::<ModelsStoreEntry>)));
    let context = radius_refresh_context(
        Some(ModelsStoreEntry {
            models: vec![serde_json::to_value(&stored_model).expect("stored model wire")],
            ..ModelsStoreEntry::default()
        }),
        false,
        None,
        Arc::clone(&log),
    );

    provider
        .refresh_models(context)
        .expect("radius supports refresh")
        .await
        .expect("restore phase succeeds");

    let ids: Vec<String> = provider
        .get_models()
        .into_iter()
        .map(|model| model.id)
        .collect();
    assert_eq!(
        ids,
        vec!["stored-model"],
        "persisted catalog replaces baseline"
    );
    let captured = log.lock().unwrap_or_else(|poison| poison.into_inner());
    assert_eq!(captured.0, 1, "the restore publication ran");
    assert!(
        captured.1.is_none(),
        "the restore phase does not rewrite the store"
    );
}

#[tokio::test]
async fn radius_refresh_fetches_the_gateway_config_and_replaces_the_catalog() {
    let config = r#"{
        "baseUrl": "https://gateway.example/v1",
        "models": [{
            "id": "gateway-model",
            "name": "Gateway Model",
            "reasoning": true,
            "input": ["text", "image"],
            "cost": {"input": 1.0, "output": 2.0, "cacheRead": 0.1, "cacheWrite": 0.2},
            "contextWindow": 100000,
            "maxTokens": 4096
        }]
    }"#;
    let (gateway, server) = serve_json_once(config);
    let provider = pir::ai::providers::radius::RadiusProvider::new(
        "radius",
        "Radius",
        vec![model("baseline-model")],
        Some(&gateway),
    )
    .expect("radius provider");
    let log = Arc::new(Mutex::new((0usize, None::<ModelsStoreEntry>)));
    let context = radius_refresh_context(
        None,
        true,
        Some(StoredCredential::ApiKey(
            pir::ai::types::ApiKeyCredential::new(Some("radius-key".into())),
        )),
        Arc::clone(&log),
    );

    provider
        .refresh_models(context)
        .expect("radius supports refresh")
        .await
        .expect("network refresh succeeds");

    let models = provider.get_models();
    assert_eq!(models.len(), 1, "{models:?}");
    let gateway_model = &models[0];
    assert_eq!(gateway_model.id, "gateway-model");
    assert_eq!(gateway_model.provider, "radius");
    assert_eq!(gateway_model.api, "pi-messages");
    assert_eq!(gateway_model.base_url, "https://gateway.example/v1");
    assert_eq!(gateway_model.context_window, 100_000);
    assert_eq!(gateway_model.max_tokens, 4096);

    let request = server.join().expect("mock server thread");
    assert!(request.starts_with("GET /v1/config "), "{request}");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer radius-key"),
        "{request}"
    );

    let captured = log.lock().unwrap_or_else(|poison| poison.into_inner());
    assert_eq!(captured.0, 1);
    let stored = captured.1.as_ref().expect("gateway catalog persisted");
    assert_eq!(stored.models.len(), 1);
    assert_eq!(stored.models[0]["id"], "gateway-model");
}

/// The persisted remote catalog is applied when the overlay is installed, not
/// only on the first background tick (a full interval out): a session restart
/// must not sit on the embedded baseline for hours while `models-store.json`
/// holds a newer catalog.
#[tokio::test]
async fn remote_catalog_overlay_restores_the_persisted_catalog_at_install() {
    use pir::ai::providers::{
        write_models_store_entry, ModelsStoreEntry, RemoteCatalogOverlay,
        RemoteCatalogOverlayConfig,
    };

    let dir = std::env::temp_dir().join(format!(
        "pir-overlay-restore-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp agent dir");
    write_models_store_entry(
        &dir,
        "anthropic",
        ModelsStoreEntry {
            models: vec![json!({
                "id": "restored-probe",
                "name": "Restored Probe",
                "api": "anthropic-messages",
                "baseUrl": "http://127.0.0.1:1",
                "input": ["text"],
                "contextWindow": 4096,
                "maxTokens": 1024
            })],
            last_modified: Some(1),
            checked_at: Some(1),
            etag: None,
        },
    )
    .expect("seed models-store");

    let registry = Arc::new(ProviderRegistry::default());
    let overlay = Arc::new(
        RemoteCatalogOverlay::new(
            RemoteCatalogOverlayConfig {
                provider_id: "anthropic".to_string(),
                // Unreachable on purpose: the restore is cache-only and the
                // first network hop is a full interval away.
                catalog_url: "http://127.0.0.1:1/api/models/providers/anthropic".to_string(),
                agent_dir: dir.clone(),
                interval: Some(Duration::from_secs(3600)),
            },
            Arc::clone(&registry),
        )
        .expect("overlay")
        // Compatibility gate: apply the restored catalog regardless of the
        // stored/generated timestamps.
        .with_local_generated_at(None),
    );
    assert!(
        overlay.spawn_refresh_task(),
        "spawn inside the test runtime"
    );
    assert_eq!(
        overlay.remote_models().len(),
        1,
        "the persisted catalog is applied at install"
    );
    assert!(
        registry.get_model("anthropic", "restored-probe").is_some(),
        "the restored catalog is registered before any network refresh"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
