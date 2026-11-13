//! Native provider/model backend registration (audit P1-01).
//!
//! A Rust [`AgentPlugin`] must be able to register a custom provider the way a
//! JS extension can: the provider is visible in the session registry, the
//! session stream/complete path dispatches through it, and unregistering or
//! ending the session removes exactly that registration. Parent/sibling
//! sessions sharing one registry and a JS/dynamic definition occupying the
//! same id must not be trampled.
//!
//! In-memory only: a scripted provider, temp session dirs, no extension
//! runtime and no network.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::error::AiResult;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{
    ModelsRefreshOptions, Provider, ProviderBox, ProviderFuture, ProviderRegistry,
    RefreshModelsContext,
};
use pir::ai::providers::{DynamicProviderConfig, RegisterDynamicProviderShared};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, ModelCost,
    ModelCostRates, StopReason, StreamOptions, TerminalReason, TextContent, ThinkingLevel, Usage,
    UserMessage, UserRole,
};
use pir::ai::virtual_::{
    PhysicalModelRef, VirtualRouteDecision, VirtualRouteFn, VirtualRouteRequest,
};
use pir::cache_warmer::{
    warm_once_with_hooks_owned, warm_once_with_hooks_owned_for_session, CacheWarmingSettings,
};
use pir::config::settings::CacheWarmingMode;
use pir::plugins::plugin::services::CompleteOptions;
use pir::plugins::plugin::PluginContext;
use pir::plugins::{AgentPlugin, PluginInfo};
use pir::sdk::AgentBuilder;
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-plugin-provider-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn model(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_owned(),
        name: id.to_owned(),
        api: "openai-chat".to_owned(),
        provider: provider.to_owned(),
        base_url: "http://127.0.0.1:9/v1".to_owned(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_owned()],
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

fn plain_context() -> Arc<Context> {
    Arc::new(Context {
        system_prompt: None,
        messages: Vec::new(),
        tools: None,
    })
}

fn message_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// The request shape one scripted stream observed, for complete/session-path
/// assertions (system prompt projection and the EFFECTIVE token cap). Like
/// the real adapters, a missing `StreamOptions.max_tokens` falls back to the
/// routed model's `max_tokens`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ScriptedRequest {
    system_prompt: Option<String>,
    max_tokens: Option<u64>,
    model_id: String,
}

/// Scripted provider: every request terminates with one text block.
struct ScriptedProvider {
    id: String,
    model: Model,
    reply: String,
    cache_write: u64,
    streams: AtomicUsize,
    refreshes: AtomicUsize,
    last_request: Mutex<Option<ScriptedRequest>>,
}

impl ScriptedProvider {
    fn new(id: &str, model: Model, reply: &str) -> Arc<Self> {
        Self::new_with_cache_write(id, model, reply, 0)
    }

    /// [`Self::new`] whose responses report a prompt-cache write, arming the
    /// cache-warming gate (`should_warm`) for warm-path tests.
    fn new_with_cache_write(id: &str, model: Model, reply: &str, cache_write: u64) -> Arc<Self> {
        Arc::new(Self {
            id: id.to_owned(),
            model,
            reply: reply.to_owned(),
            cache_write,
            streams: AtomicUsize::new(0),
            refreshes: AtomicUsize::new(0),
            last_request: Mutex::new(None),
        })
    }

    fn streams(&self) -> usize {
        self.streams.load(Ordering::SeqCst)
    }

    fn refreshes(&self) -> usize {
        self.refreshes.load(Ordering::SeqCst)
    }

    fn last_request(&self) -> Option<ScriptedRequest> {
        self.last_request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.id
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }

    /// SNAP10: a refreshable provider whose refresh is observable per
    /// allocation, so a test can prove `refresh_for_session` selected the
    /// session-owned entry instead of the process-shared last writer.
    fn supports_refresh(&self) -> bool {
        true
    }

    fn refresh_models(
        &self,
        _context: RefreshModelsContext,
    ) -> Option<ProviderFuture<AiResult<()>>> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Some(Box::pin(async { Ok(()) }))
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.streams.fetch_add(1, Ordering::SeqCst);
        *self
            .last_request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ScriptedRequest {
            system_prompt: context.system_prompt,
            max_tokens: options
                .max_tokens
                .or((model.max_tokens > 0).then_some(model.max_tokens)),
            model_id: model.id.clone(),
        });
        let message = AssistantMessage {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            stop_reason: StopReason::Stop,
            content: vec![ContentBlock::Text(TextContent {
                text: self.reply.clone(),
                text_signature: None,
            })],
            usage: Usage {
                cache_write: self.cache_write,
                total_tokens: self.cache_write,
                ..Usage::default()
            },
            ..AssistantMessage::default()
        };
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

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
}

/// Erase a scripted provider to the boxed ABI type.
fn boxed_provider(provider: Arc<ScriptedProvider>) -> ProviderBox {
    provider
}

/// A plugin whose `on_context` runs a caller-supplied registration closure.
struct OnContextPlugin {
    info: PluginInfo,
    on_context: Box<dyn Fn(&PluginContext) + Send + Sync>,
}

impl OnContextPlugin {
    fn new(name: &str, on_context: impl Fn(&PluginContext) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            on_context: Box::new(on_context),
        })
    }
}

impl AgentPlugin for OnContextPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn on_context(&self, ctx: &PluginContext) {
        (self.on_context)(ctx);
    }
}

type ContextSlot = Arc<Mutex<Option<PluginContext>>>;

fn context_slot() -> ContextSlot {
    Arc::new(Mutex::new(None))
}

fn take_context(slot: &ContextSlot) -> PluginContext {
    // Actually TAKE the clone: leaving it in the slot would keep the live
    // services (and their registry registrations) alive past `drop(ctx)` and
    // make teardown assertions depend on the slot's later drop.
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("on_context captured a context")
}

fn base_registry() -> (Arc<ProviderRegistry>, Model) {
    let session_model = model("session-provider", "session-model");
    let mut registry = ProviderRegistry::default();
    registry.set_provider(ScriptedProvider::new(
        "session-provider",
        session_model.clone(),
        "session",
    ));
    (Arc::new(registry), session_model)
}

fn session_config(
    temp: &TempDir,
    registry: Arc<ProviderRegistry>,
    model: Model,
) -> AgentSessionConfig {
    let cwd = temp.path().to_str().expect("utf-8 temp path").to_owned();
    let session_dir = temp.path().join("sessions");
    AgentSessionConfig {
        performance: Default::default(),
        provider_registry: registry,
        model,
        api_key: None,
        thinking: ThinkingLevel::Low,
        tools: ToolRegistry::new(),
        session: Some(SessionManager::create(&cwd, &session_dir).expect("create session")),
        compaction: None,
        max_turns: 2,
        system_prompt: Some("provider backend test prompt".to_owned()),
        cwd: Some(temp.path().to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

fn session_with(
    temp: &TempDir,
    registry: Arc<ProviderRegistry>,
    model: Model,
    plugins: Vec<Arc<dyn AgentPlugin>>,
) -> Arc<AgentSession> {
    AgentSession::new_arc_with_plugins(session_config(temp, registry, model), plugins)
}

/// [`session_with`] against a caller-built session manager (the branch-summary
/// tests need a pre-populated tree).
fn session_with_manager(
    temp: &TempDir,
    registry: Arc<ProviderRegistry>,
    model: Model,
    session: SessionManager,
    plugins: Vec<Arc<dyn AgentPlugin>>,
) -> Arc<AgentSession> {
    let mut config = session_config(temp, registry, model);
    config.session = Some(session);
    AgentSession::new_arc_with_plugins(config, plugins)
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        api: "openai-chat".to_owned(),
        provider: "branch-fixture".to_owned(),
        model: "branch-fixture".to_owned(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 2,
        ..AssistantMessage::default()
    })
}

/// Linear `u1 -> a1` plus a sibling branch `u1 -> u2 -> u3`, the shape
/// `navigate_tree` summarizes when returning from `u3` to `u1`.
fn branched_session() -> (SessionManager, Vec<String>) {
    let mut session = SessionManager::in_memory(".", None).expect("in-memory session");
    let u1 = session
        .append_message(user_message("first"))
        .expect("append u1");
    let a1 = session
        .append_message(assistant_message("answer"))
        .expect("append a1");
    session.branch(&u1).expect("branch");
    let u2 = session
        .append_message(user_message("second"))
        .expect("append u2");
    let u3 = session
        .append_message(user_message("third"))
        .expect("append u3");
    (session, vec![u1, a1, u2, u3])
}

/// Poll until `provider` observed `expected` requests (cache warming runs on a
/// detached task, like the production path).
async fn wait_for_streams(provider: &ScriptedProvider, expected: usize) {
    for _ in 0..200 {
        if provider.streams() >= expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn registry_provider_ids(registry: &ProviderRegistry) -> Vec<String> {
    registry
        .providers()
        .iter()
        .map(|provider| provider.id().to_owned())
        .collect()
}

/// Runtime-JSON provider config with one model, used by the restore-chain
/// tests. The model id identifies the owner in assertions.
fn json_provider_config(model_id: &str, name: &str) -> serde_json::Value {
    json!({
        "baseUrl": "http://127.0.0.1:9/v1",
        "api": "openai-completions",
        "models": [{
            "id": model_id,
            "name": name,
            "contextWindow": 2048,
            "maxTokens": 128
        }]
    })
}

fn shared_json_model_ids(registry: &ProviderRegistry) -> Vec<String> {
    registry
        .get_provider_models("shared-json")
        .into_iter()
        .map(|entry| entry.id)
        .collect()
}

/// Native virtual-model definition for the shared `virtual-provider` key.
fn shared_virtual_definition(
    name: &str,
    physical_provider: &str,
    physical_id: &str,
) -> serde_json::Value {
    json!({
        "id": "shared-virtual",
        "provider": "virtual-provider",
        "name": name,
        "physical": { "provider": physical_provider, "id": physical_id }
    })
}

fn shared_virtual_name(registry: &ProviderRegistry) -> Option<String> {
    registry
        .get_model("virtual-provider", "shared-virtual")
        .map(|model| model.name)
}

/// Route callback that counts invocations and keeps the static route. Used to
/// prove a displaced owner's callback is cleared while a child owns the key
/// and restored only for a live owner.
fn counting_virtual_route(calls: Arc<AtomicUsize>) -> VirtualRouteFn {
    Arc::new(
        move |_request: VirtualRouteRequest| -> pir::ai::virtual_::VirtualRouteFuture {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(VirtualRouteDecision {
                    physical: PhysicalModelRef {
                        provider: "session-provider".to_owned(),
                        id: "session-model".to_owned(),
                    },
                    thinking_level: None,
                    state: None,
                })
            })
        },
    )
}

// ---------------------------------------------------------------------------
// Typed provider: register -> visible -> complete/stream -> unregister
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn typed_provider_registers_visible_completes_and_unregisters() {
    let temp = TempDir::new("typed");
    let (registry, session_model) = base_registry();
    let provider = ScriptedProvider::new(
        "native-echo",
        model("native-echo", "echo-1"),
        "hello from native",
    );
    let slot = context_slot();
    let plugin = OnContextPlugin::new("native-provider-plugin", {
        let provider = boxed_provider(Arc::clone(&provider));
        let slot = Arc::clone(&slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let session = session_with(&temp, Arc::clone(&registry), session_model, vec![plugin]);
    let ctx = take_context(&slot);
    let service = ctx.require_providers().expect("providers bound");

    assert!(
        registry_provider_ids(&registry)
            .iter()
            .any(|id| id == "native-echo"),
        "typed provider must be visible in the session registry"
    );
    assert_eq!(registry.get_provider_models("native-echo").len(), 1);
    let listed = registry
        .get_model("native-echo", "echo-1")
        .expect("typed model is listed");
    assert_eq!(listed.provider, "native-echo");

    // ProviderService::complete is the native modelRegistry.complete.
    let completed = service
        .complete(
            &model("native-echo", "echo-1"),
            plain_context(),
            CompleteOptions {
                max_tokens: Some(64),
                temperature: Some(0.0),
                signal: None,
            },
        )
        .await
        .expect("service complete");
    assert_eq!(message_text(&completed), "hello from native");
    assert_eq!(completed.provider, "native-echo");
    assert_eq!(completed.stop_reason, StopReason::Stop);

    // The registry's own stream path (what the agent loop uses) routes to the
    // same typed provider.
    let stream = registry.stream_resolving_auth(
        model("native-echo", "echo-1"),
        plain_context(),
        StreamOptions::default(),
    );
    let streamed = stream.drain_and_result().await.expect("stream terminated");
    assert_eq!(message_text(&streamed), "hello from native");
    assert!(provider.streams() >= 2, "both paths invoked the provider");

    // Unregister removes the provider and its catalog entries; complete now
    // fails closed instead of fabricating a message.
    service.unregister_provider("native-echo");
    assert!(registry.plugin_provider("native-echo").is_none());
    assert!(registry.get_provider_models("native-echo").is_empty());
    assert!(!registry_provider_ids(&registry)
        .iter()
        .any(|id| id == "native-echo"));
    let error = service
        .complete(
            &model("native-echo", "echo-1"),
            plain_context(),
            CompleteOptions::default(),
        )
        .await
        .expect_err("unknown model fails closed");
    assert!(
        error.to_string().contains("unknown model"),
        "expected the completion boundary error, got {error}"
    );

    // Moving the session out of the picture keeps the registry test handle
    // alive; the context slot is the only holder of the service in this test.
    drop(ctx);
    drop(session);
}

// ---------------------------------------------------------------------------
// Session teardown removes exactly the typed registrations it installed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn typed_provider_is_removed_on_session_teardown() {
    let temp = TempDir::new("typed-cleanup");
    let (registry, session_model) = base_registry();
    let provider = ScriptedProvider::new(
        "cleanup-typed",
        model("cleanup-typed", "cleanup-model"),
        "cleanup",
    );
    let plugin = OnContextPlugin::new("cleanup-plugin", {
        let provider = boxed_provider(Arc::clone(&provider));
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
        }
    });
    let session = session_with(&temp, Arc::clone(&registry), session_model, vec![plugin]);
    assert!(registry.plugin_provider("cleanup-typed").is_some());

    drop(session);
    assert!(
        registry.plugin_provider("cleanup-typed").is_none(),
        "the service drop must retire its typed provider"
    );
    assert!(registry.get_provider_models("cleanup-typed").is_empty());
}

// ---------------------------------------------------------------------------
// Parent / sibling sessions sharing one registry
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn sibling_typed_registrations_stack_and_teardown_reveals_the_parent() {
    let temp = TempDir::new("sibling-stack");
    let (registry, session_model) = base_registry();
    let parent_provider = ScriptedProvider::new(
        "shared-native",
        model("shared-native", "parent-model"),
        "parent",
    );
    let child_provider = ScriptedProvider::new(
        "shared-native",
        model("shared-native", "child-model"),
        "child",
    );
    let parent_plugin = OnContextPlugin::new("parent-register", {
        let provider = boxed_provider(Arc::clone(&parent_provider));
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
        }
    });
    let child_plugin = OnContextPlugin::new("child-register", {
        let provider = boxed_provider(Arc::clone(&child_provider));
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
        }
    });

    let parent = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![parent_plugin],
    );
    assert!(registry
        .get_model("shared-native", "parent-model")
        .is_some());
    let child = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![child_plugin],
    );
    assert!(
        registry.get_model("shared-native", "child-model").is_some(),
        "the newest registration wins while its owner is live"
    );

    drop(child);
    assert!(
        registry
            .get_model("shared-native", "parent-model")
            .is_some(),
        "dropping the child must reveal the parent's registration, not delete it"
    );
    assert!(registry.get_model("shared-native", "child-model").is_none());

    drop(parent);
    assert!(registry.plugin_provider("shared-native").is_none());
}

// ---------------------------------------------------------------------------
// JS / shared dynamic registry coexistence
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn typed_provider_does_not_delete_the_shared_dynamic_registration() {
    let temp = TempDir::new("js-coexist");
    let (registry, session_model) = base_registry();
    registry
        .register_dynamic_provider_shared(
            "shared-registry",
            DynamicProviderConfig {
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                api: "openai-completions".to_owned(),
                models: vec![model("", "js-model")],
                ..DynamicProviderConfig::default()
            },
        )
        .expect("JS-style dynamic registration");
    let provider = ScriptedProvider::new(
        "shared-registry",
        model("shared-registry", "native-model"),
        "native",
    );
    let slot = context_slot();
    let plugin = OnContextPlugin::new("native-overlay", {
        let provider = boxed_provider(Arc::clone(&provider));
        let slot = Arc::clone(&slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let session = session_with(&temp, Arc::clone(&registry), session_model, vec![plugin]);
    let ctx = take_context(&slot);
    let service = ctx.require_providers().expect("providers bound");

    // The typed provider outranks the JSON/dynamic entry while it is live, and
    // exactly one provider row is listed (no duplicate model catalogs).
    assert_eq!(
        registry
            .get_provider_models("shared-registry")
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>(),
        vec!["native-model".to_owned()]
    );
    assert_eq!(
        registry_provider_ids(&registry)
            .iter()
            .filter(|id| id.as_str() == "shared-registry")
            .count(),
        1
    );

    service.unregister_provider("shared-registry");
    assert_eq!(
        registry
            .get_provider_models("shared-registry")
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>(),
        vec!["js-model".to_owned()],
        "removing the native overlay must reveal the JS/shared entry"
    );

    // Re-register and let the session end: the shared entry still survives.
    service.register_provider_impl(boxed_provider(Arc::clone(&provider)));
    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    drop(ctx);
    drop(session);
    assert_eq!(
        registry
            .get_provider_models("shared-registry")
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>(),
        vec!["js-model".to_owned()],
        "session teardown must not delete a JS/shared dynamic registration"
    );
}

// ---------------------------------------------------------------------------
// Runtime JSON register_provider / unregister_provider
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn runtime_json_provider_registration_is_live_and_owned() {
    let temp = TempDir::new("json-runtime");
    let (registry, session_model) = base_registry();
    let slot = context_slot();
    let config = json!({
        "baseUrl": "http://127.0.0.1:9/v1",
        "api": "openai-completions",
        "models": [{
            "id": "json-model",
            "name": "JSON Model",
            "contextWindow": 2048,
            "maxTokens": 128
        }]
    });
    let plugin = OnContextPlugin::new("json-provider", {
        let slot = Arc::clone(&slot);
        let config = config.clone();
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider("runtime-json", config.clone());
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let session = session_with(&temp, Arc::clone(&registry), session_model, vec![plugin]);
    let ctx = take_context(&slot);
    let service = ctx.require_providers().expect("providers bound");

    assert_eq!(
        registry
            .get_provider_models("runtime-json")
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>(),
        vec!["json-model".to_owned()]
    );

    service.unregister_provider("runtime-json");
    assert!(registry.get_provider_models("runtime-json").is_empty());

    service.register_provider("runtime-json", config);
    assert_eq!(registry.get_provider_models("runtime-json").len(), 1);

    // Session teardown removes this binding's runtime JSON registration.
    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    drop(ctx);
    drop(session);
    assert!(registry.get_provider_models("runtime-json").is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_json_child_teardown_restores_the_parent_registration() {
    let temp = TempDir::new("json-parent-child");
    let (registry, session_model) = base_registry();
    let json_config = |model_id: &str, name: &str| {
        json!({
            "baseUrl": "http://127.0.0.1:9/v1",
            "api": "openai-completions",
            "models": [{
                "id": model_id,
                "name": name,
                "contextWindow": 2048,
                "maxTokens": 128
            }]
        })
    };
    let parent_plugin = OnContextPlugin::new("json-parent", {
        let config = json_config("parent-json-model", "Parent JSON Model");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider("shared-json", config.clone());
        }
    });
    let child_plugin = OnContextPlugin::new("json-child", {
        let config = json_config("child-json-model", "Child JSON Model");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider("shared-json", config.clone());
        }
    });

    let parent = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![parent_plugin],
    );
    assert_eq!(
        registry
            .get_provider_models("shared-json")
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>(),
        vec!["parent-json-model".to_owned()]
    );

    let child = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![child_plugin],
    );
    assert_eq!(
        registry
            .get_provider_models("shared-json")
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>(),
        vec!["child-json-model".to_owned()],
        "the newest JSON registration wins while its owner is live"
    );

    drop(child);
    assert_eq!(
        registry
            .get_provider_models("shared-json")
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>(),
        vec!["parent-json-model".to_owned()],
        "child teardown must restore the parent's displaced JSON entry"
    );

    drop(parent);
    assert!(
        registry.get_provider_models("shared-json").is_empty(),
        "the parent was the first owner, so its teardown removes the key"
    );
}

/// P2-REVIEW2-01: a parent that tears down FIRST must not be resurrected when
/// the still-live child later tears down. The child's restore chain records
/// the parent as its owner, not merely a strong `Option<ProviderBox>`.
#[tokio::test(flavor = "current_thread")]
async fn runtime_json_parent_first_teardown_does_not_resurrect_the_dead_parent() {
    let temp = TempDir::new("json-parent-first");
    let (registry, session_model) = base_registry();
    let parent_plugin = OnContextPlugin::new("json-parent-first", {
        let config = json_provider_config("parent-json-model", "Parent JSON Model");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider("shared-json", config.clone());
        }
    });
    let child_plugin = OnContextPlugin::new("json-child-first", {
        let config = json_provider_config("child-json-model", "Child JSON Model");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider("shared-json", config.clone());
        }
    });

    let parent = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![parent_plugin],
    );
    let child = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![child_plugin],
    );
    assert_eq!(
        shared_json_model_ids(&registry),
        vec!["child-json-model".to_owned()]
    );

    drop(parent);
    assert_eq!(
        shared_json_model_ids(&registry),
        vec!["child-json-model".to_owned()],
        "the parent's teardown must not touch the live child's installation"
    );

    drop(child);
    assert!(
        shared_json_model_ids(&registry).is_empty(),
        "the child's teardown must skip the dead parent owner and remove the key"
    );
}

/// P2-REVIEW2-01: a parent re-registration while the child owns the id must
/// not make the child restore a dead intermediate on teardown.
#[tokio::test(flavor = "current_thread")]
async fn runtime_json_parent_reregistration_does_not_restore_a_dead_intermediate() {
    let temp = TempDir::new("json-parent-reregister");
    let (registry, session_model) = base_registry();
    let parent_slot = context_slot();
    let parent_plugin = OnContextPlugin::new("json-parent-reregister", {
        let slot = Arc::clone(&parent_slot);
        let config = json_provider_config("parent-json-model", "Parent JSON Model");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider("shared-json", config.clone());
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let child_plugin = OnContextPlugin::new("json-child-reregister", {
        let config = json_provider_config("child-json-model", "Child JSON Model");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider("shared-json", config.clone());
        }
    });

    let parent = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![parent_plugin],
    );
    let parent_ctx = take_context(&parent_slot);
    let parent_service = parent_ctx.require_providers().expect("providers bound");
    let child = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![child_plugin],
    );
    assert_eq!(
        shared_json_model_ids(&registry),
        vec!["child-json-model".to_owned()]
    );

    parent_service.register_provider(
        "shared-json",
        json_provider_config("parent-2-json-model", "Parent 2 JSON Model"),
    );
    assert_eq!(
        shared_json_model_ids(&registry),
        vec!["parent-2-json-model".to_owned()],
        "the parent's re-registration wins while it is live"
    );

    drop(parent_ctx);
    drop(parent);
    assert_eq!(
        shared_json_model_ids(&registry),
        vec!["child-json-model".to_owned()],
        "the parent teardown restores the live child it displaced last"
    );

    drop(child);
    assert!(
        shared_json_model_ids(&registry).is_empty(),
        "the child teardown must not restore the dead parent's first registration"
    );
}

/// P2-REVIEW2-01: non-LIFO teardown (middle sibling first) restores the
/// nearest LIVE owner, not the most recently displaced dead sibling.
#[tokio::test(flavor = "current_thread")]
async fn runtime_json_non_lifo_teardown_restores_the_nearest_live_owner() {
    let temp = TempDir::new("json-non-lifo");
    let (registry, session_model) = base_registry();
    let register_plugin = |name: &'static str, model_id: &'static str, model_name: &'static str| {
        OnContextPlugin::new(name, move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider("shared-json", json_provider_config(model_id, model_name));
        })
    };

    let first = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![register_plugin(
            "json-non-lifo-first",
            "first-json-model",
            "First JSON Model",
        )],
    );
    let middle = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![register_plugin(
            "json-non-lifo-middle",
            "middle-json-model",
            "Middle JSON Model",
        )],
    );
    let last = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![register_plugin(
            "json-non-lifo-last",
            "last-json-model",
            "Last JSON Model",
        )],
    );
    assert_eq!(
        shared_json_model_ids(&registry),
        vec!["last-json-model".to_owned()]
    );

    drop(middle);
    assert_eq!(
        shared_json_model_ids(&registry),
        vec!["last-json-model".to_owned()],
        "removing a displaced sibling must not touch the current owner"
    );

    drop(last);
    assert_eq!(
        shared_json_model_ids(&registry),
        vec!["first-json-model".to_owned()],
        "the last owner's chain must skip the finished middle sibling and restore the live first owner"
    );

    drop(first);
    assert!(
        shared_json_model_ids(&registry).is_empty(),
        "the first owner was the root, so its teardown removes the key"
    );
}

// ---------------------------------------------------------------------------
// Virtual model registration / unregistration / teardown
// ---------------------------------------------------------------------------

/// SNAP7 P2-2A: the native runtime-JSON path must remember the dynamic entry
/// it displaced, not only the identity it installed. Both an explicit
/// `unregister_provider` and session teardown restore that entry.
#[tokio::test(flavor = "current_thread")]
async fn runtime_json_unregister_and_teardown_restore_the_displaced_dynamic_entry() {
    let temp = TempDir::new("json-runtime-restore");
    let (registry, session_model) = base_registry();
    let slot = context_slot();
    registry
        .register_dynamic_provider_shared(
            "runtime-json",
            DynamicProviderConfig {
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                api: "openai-completions".to_owned(),
                models: vec![model("", "js-model")],
                ..DynamicProviderConfig::default()
            },
        )
        .expect("JS-style dynamic registration");
    let json_config = json!({
        "baseUrl": "http://127.0.0.1:9/v1",
        "api": "openai-completions",
        "models": [{
            "id": "json-model",
            "name": "JSON Model",
            "contextWindow": 2048,
            "maxTokens": 128
        }]
    });
    let plugin = OnContextPlugin::new("json-restore", {
        let slot = Arc::clone(&slot);
        let config = json_config.clone();
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider("runtime-json", config.clone());
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let session = session_with(&temp, Arc::clone(&registry), session_model, vec![plugin]);
    let ctx = take_context(&slot);
    let service = ctx.require_providers().expect("providers bound");
    let model_ids = || {
        registry
            .get_provider_models("runtime-json")
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>()
    };

    assert_eq!(model_ids(), vec!["json-model".to_owned()]);

    service.unregister_provider("runtime-json");
    assert_eq!(
        model_ids(),
        vec!["js-model".to_owned()],
        "unregister must restore the displaced dynamic entry"
    );

    service.register_provider("runtime-json", json_config);
    assert_eq!(model_ids(), vec!["json-model".to_owned()]);

    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    drop(ctx);
    drop(session);
    assert_eq!(
        model_ids(),
        vec!["js-model".to_owned()],
        "session teardown must restore the displaced dynamic entry"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn virtual_model_registration_is_live_and_owned() {
    let temp = TempDir::new("virtual-model");
    let (registry, session_model) = base_registry();
    let slot = context_slot();
    let definition = json!({
        "id": "virtual-smart",
        "provider": "virtual-provider",
        "name": "Virtual Smart",
        "physical": { "provider": "session-provider", "id": "session-model" }
    });
    let plugin = OnContextPlugin::new("virtual-model-plugin", {
        let slot = Arc::clone(&slot);
        let definition = definition.clone();
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_virtual_model(definition.clone());
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let session = session_with(&temp, Arc::clone(&registry), session_model, vec![plugin]);
    let ctx = take_context(&slot);
    let service = ctx.require_providers().expect("providers bound");

    let virtual_model = registry
        .get_model("virtual-provider", "virtual-smart")
        .expect("virtual model is listed");
    assert_eq!(virtual_model.api, pir::ai::virtual_::VIRTUAL_MODEL_API);
    let physical = service
        .physical_model("session-provider", "session-model")
        .expect("physical model resolves");
    assert_eq!(physical.id, "session-model");
    assert_eq!(physical.provider, "session-provider");

    // Completing a virtual model routes to its physical provider, never to a
    // provider named after the virtual catalog entry. Use the registry's own
    // `pi-virtual` catalog model (the selection the session would hold), not
    // a hand-built `openai-chat` model: only the former exercises the real
    // virtual registration path.
    let completed = service
        .complete(&virtual_model, plain_context(), CompleteOptions::default())
        .await
        .expect("virtual model completes through its physical route");
    assert_eq!(completed.provider, "session-provider");
    assert_eq!(message_text(&completed), "session");

    service.unregister_virtual_model("virtual-provider", "virtual-smart");
    assert!(registry
        .get_model("virtual-provider", "virtual-smart")
        .is_none());

    service.register_virtual_model(definition);
    assert!(registry
        .get_model("virtual-provider", "virtual-smart")
        .is_some());

    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    drop(ctx);
    drop(session);
    assert!(
        registry
            .get_model("virtual-provider", "virtual-smart")
            .is_none(),
        "session teardown removes the binding's virtual model"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn virtual_model_child_teardown_restores_the_parent_registration() {
    let temp = TempDir::new("virtual-parent-child");
    let (registry, session_model) = base_registry();
    let definition = |name: &str| {
        json!({
            "id": "shared-virtual",
            "provider": "virtual-provider",
            "name": name,
            "physical": { "provider": "session-provider", "id": "session-model" }
        })
    };
    let parent_plugin = OnContextPlugin::new("virtual-parent", {
        let definition = definition("Parent Virtual");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_virtual_model(definition.clone());
        }
    });
    let child_plugin = OnContextPlugin::new("virtual-child", {
        let definition = definition("Child Virtual");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_virtual_model(definition.clone());
        }
    });

    let parent = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![parent_plugin],
    );
    assert_eq!(
        registry
            .get_model("virtual-provider", "shared-virtual")
            .expect("parent virtual model")
            .name,
        "Parent Virtual"
    );

    let child = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![child_plugin],
    );
    assert_eq!(
        registry
            .get_model("virtual-provider", "shared-virtual")
            .expect("child virtual model")
            .name,
        "Child Virtual",
        "the newest virtual registration wins while its owner is live"
    );

    drop(child);
    assert_eq!(
        registry
            .get_model("virtual-provider", "shared-virtual")
            .expect("parent virtual model restored")
            .name,
        "Parent Virtual",
        "child teardown must restore the parent's displaced virtual entry"
    );

    drop(parent);
    assert!(
        registry
            .get_model("virtual-provider", "shared-virtual")
            .is_none(),
        "the parent was the first owner, so its teardown removes the key"
    );
}

/// P2-REVIEW2-02: a virtual registration installed by a parent that tears
/// down FIRST must be skipped by the child's restore walk. The parent's route
/// callback must not survive under the child or reappear after the child ends.
#[tokio::test(flavor = "current_thread")]
async fn virtual_model_parent_first_teardown_does_not_resurrect_dead_registration() {
    let temp = TempDir::new("virtual-parent-first");
    let (registry, session_model) = base_registry();
    let parent_plugin = OnContextPlugin::new("virtual-parent-first", {
        let definition =
            shared_virtual_definition("Parent Virtual", "session-provider", "session-model");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_virtual_model(definition.clone());
        }
    });
    let child_slot = context_slot();
    let child_plugin = OnContextPlugin::new("virtual-child-first", {
        let slot = Arc::clone(&child_slot);
        let definition =
            shared_virtual_definition("Child Virtual", "session-provider", "session-model");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_virtual_model(definition.clone());
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let route_calls = Arc::new(AtomicUsize::new(0));

    let parent = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![parent_plugin],
    );
    registry.virtual_model_registry().set_route_callback(
        "virtual-provider",
        "shared-virtual",
        Some(counting_virtual_route(Arc::clone(&route_calls))),
    );
    let child = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![child_plugin],
    );
    assert_eq!(
        shared_virtual_name(&registry).as_deref(),
        Some("Child Virtual")
    );
    assert!(
        registry
            .virtual_model_registry()
            .route_callback("virtual-provider", "shared-virtual")
            .is_none(),
        "a native child registration must clear the displaced parent callback"
    );

    drop(parent);
    assert_eq!(
        shared_virtual_name(&registry).as_deref(),
        Some("Child Virtual"),
        "the parent's teardown must not touch the live child"
    );

    let child_ctx = take_context(&child_slot);
    let child_service = child_ctx.require_providers().expect("providers bound");
    let child_model = registry
        .get_model("virtual-provider", "shared-virtual")
        .expect("child virtual model");
    let completed = child_service
        .complete(&child_model, plain_context(), CompleteOptions::default())
        .await
        .expect("child virtual completion");
    assert_eq!(message_text(&completed), "session");
    assert_eq!(
        route_calls.load(Ordering::SeqCst),
        0,
        "the dead parent's route callback must not decide the child's request"
    );

    drop(child_ctx);
    drop(child);
    assert!(
        shared_virtual_name(&registry).is_none(),
        "the child's teardown must skip the dead parent and remove the key"
    );
    assert!(
        registry
            .virtual_model_registry()
            .route_callback("virtual-provider", "shared-virtual")
            .is_none(),
        "no callback may be left behind for the removed key"
    );
}

/// P2-REVIEW2-02: child teardown restores both the live parent registration
/// and its route callback; while the child owns the key the parent callback
/// must be inactive.
#[tokio::test(flavor = "current_thread")]
async fn virtual_model_child_teardown_restores_the_live_parent_and_route_callback() {
    let temp = TempDir::new("virtual-route-restore");
    let (registry, session_model) = base_registry();
    let parent_slot = context_slot();
    let parent_plugin = OnContextPlugin::new("virtual-route-parent", {
        let slot = Arc::clone(&parent_slot);
        let definition =
            shared_virtual_definition("Parent Virtual", "session-provider", "session-model");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_virtual_model(definition.clone());
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let child_plugin = OnContextPlugin::new("virtual-route-child", {
        let definition =
            shared_virtual_definition("Child Virtual", "session-provider", "session-model");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_virtual_model(definition.clone());
        }
    });
    let route_calls = Arc::new(AtomicUsize::new(0));

    let parent = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![parent_plugin],
    );
    let parent_ctx = take_context(&parent_slot);
    registry.virtual_model_registry().set_route_callback(
        "virtual-provider",
        "shared-virtual",
        Some(counting_virtual_route(Arc::clone(&route_calls))),
    );
    let child = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![child_plugin],
    );
    assert_eq!(
        shared_virtual_name(&registry).as_deref(),
        Some("Child Virtual")
    );
    assert!(
        registry
            .virtual_model_registry()
            .route_callback("virtual-provider", "shared-virtual")
            .is_none(),
        "native child registration clears the parent's callback"
    );

    drop(child);
    assert_eq!(
        shared_virtual_name(&registry).as_deref(),
        Some("Parent Virtual"),
        "child teardown restores the live parent registration"
    );
    assert!(
        registry
            .virtual_model_registry()
            .route_callback("virtual-provider", "shared-virtual")
            .is_some(),
        "child teardown restores the parent's route callback"
    );
    let parent_service = parent_ctx.require_providers().expect("providers bound");
    let parent_model = registry
        .get_model("virtual-provider", "shared-virtual")
        .expect("parent virtual model");
    let completed = parent_service
        .complete(&parent_model, plain_context(), CompleteOptions::default())
        .await
        .expect("parent virtual completion");
    assert_eq!(message_text(&completed), "session");
    assert_eq!(
        route_calls.load(Ordering::SeqCst),
        1,
        "the restored parent callback decides the parent's request again"
    );

    drop(parent_ctx);
    drop(parent);
    assert!(shared_virtual_name(&registry).is_none());
    assert!(registry
        .virtual_model_registry()
        .route_callback("virtual-provider", "shared-virtual")
        .is_none());
}

/// P2-REVIEW2-02: a parent re-registration while the child owns the key must
/// not leave the child restoring the parent's dead first registration.
#[tokio::test(flavor = "current_thread")]
async fn virtual_model_parent_reregistration_does_not_restore_a_dead_intermediate() {
    let temp = TempDir::new("virtual-parent-reregister");
    let (registry, session_model) = base_registry();
    let parent_slot = context_slot();
    let parent_plugin = OnContextPlugin::new("virtual-reregister-parent", {
        let slot = Arc::clone(&parent_slot);
        let definition =
            shared_virtual_definition("Parent Virtual", "session-provider", "session-model");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_virtual_model(definition.clone());
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let child_plugin = OnContextPlugin::new("virtual-reregister-child", {
        let definition =
            shared_virtual_definition("Child Virtual", "session-provider", "session-model");
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_virtual_model(definition.clone());
        }
    });

    let parent = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![parent_plugin],
    );
    let parent_ctx = take_context(&parent_slot);
    let parent_service = parent_ctx.require_providers().expect("providers bound");
    let child = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![child_plugin],
    );
    assert_eq!(
        shared_virtual_name(&registry).as_deref(),
        Some("Child Virtual")
    );

    parent_service.register_virtual_model(shared_virtual_definition(
        "Parent 2 Virtual",
        "session-provider",
        "session-model",
    ));
    assert_eq!(
        shared_virtual_name(&registry).as_deref(),
        Some("Parent 2 Virtual")
    );

    drop(parent_ctx);
    drop(parent);
    assert_eq!(
        shared_virtual_name(&registry).as_deref(),
        Some("Child Virtual"),
        "the parent teardown restores the live child it displaced last"
    );

    drop(child);
    assert!(
        shared_virtual_name(&registry).is_none(),
        "the child teardown must not restore the dead parent's first registration"
    );
}

/// P2-REVIEW2-02: non-LIFO virtual teardown restores the nearest live owner.
#[tokio::test(flavor = "current_thread")]
async fn virtual_model_non_lifo_teardown_restores_the_nearest_live_registration() {
    let temp = TempDir::new("virtual-non-lifo");
    let (registry, session_model) = base_registry();
    let register_plugin = |name: &'static str, model_name: &'static str| {
        OnContextPlugin::new(name, move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_virtual_model(shared_virtual_definition(
                    model_name,
                    "session-provider",
                    "session-model",
                ));
        })
    };

    let first = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![register_plugin("virtual-non-lifo-first", "First Virtual")],
    );
    let middle = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![register_plugin("virtual-non-lifo-middle", "Middle Virtual")],
    );
    let last = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![register_plugin("virtual-non-lifo-last", "Last Virtual")],
    );
    assert_eq!(
        shared_virtual_name(&registry).as_deref(),
        Some("Last Virtual")
    );

    drop(middle);
    assert_eq!(
        shared_virtual_name(&registry).as_deref(),
        Some("Last Virtual"),
        "removing a displaced sibling must not touch the current owner"
    );

    drop(last);
    assert_eq!(
        shared_virtual_name(&registry).as_deref(),
        Some("First Virtual"),
        "the last owner's chain must skip the finished middle sibling and restore the live first owner"
    );

    drop(first);
    assert!(shared_virtual_name(&registry).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn virtual_complete_applies_route_callback_prompt_and_max_tokens_override() {
    let temp = TempDir::new("virtual-complete-projection");
    let (registry, session_model) = base_registry();
    let route_provider = ScriptedProvider::new(
        "route-provider",
        model("route-provider", "route-model"),
        "routed",
    );
    let route_calls = Arc::new(AtomicUsize::new(0));
    let seen_request = Arc::new(Mutex::new(None::<VirtualRouteRequest>));
    let definition = json!({
        "id": "virtual-routed",
        "provider": "virtual-provider",
        "name": "Virtual Routed",
        "physical": { "provider": "session-provider", "id": "session-model" },
        "systemPrompt": "virtual system",
        "modelOverride": { "contextWindow": 2048, "maxTokens": 77 }
    });
    let slot = context_slot();
    let plugin = OnContextPlugin::new("virtual-projection", {
        let slot = Arc::clone(&slot);
        let definition = definition.clone();
        let route_provider = boxed_provider(Arc::clone(&route_provider));
        move |ctx| {
            let providers = ctx.require_providers().expect("providers bound");
            providers.register_provider_impl(Arc::clone(&route_provider));
            providers.register_virtual_model(definition.clone());
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let session = session_with(&temp, Arc::clone(&registry), session_model, vec![plugin]);
    let ctx = take_context(&slot);
    let service = ctx.require_providers().expect("providers bound");

    let callback: VirtualRouteFn = {
        let route_calls = Arc::clone(&route_calls);
        let seen_request = Arc::clone(&seen_request);
        Arc::new(
            move |request: VirtualRouteRequest| -> pir::ai::virtual_::VirtualRouteFuture {
                route_calls.fetch_add(1, Ordering::SeqCst);
                *seen_request
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(request);
                Box::pin(async {
                    Ok(VirtualRouteDecision {
                        physical: PhysicalModelRef {
                            provider: "route-provider".to_owned(),
                            id: "route-model".to_owned(),
                        },
                        thinking_level: None,
                        state: None,
                    })
                })
            },
        )
    };
    registry.virtual_model_registry().set_route_callback(
        "virtual-provider",
        "virtual-routed",
        Some(callback),
    );

    let virtual_model = registry
        .get_model("virtual-provider", "virtual-routed")
        .expect("virtual model is listed");
    let context = Arc::new(Context {
        system_prompt: Some("base prompt".to_owned()),
        messages: Vec::new(),
        tools: None,
    });
    let completed = service
        .complete(&virtual_model, context, CompleteOptions::default())
        .await
        .expect("virtual model completes through its route callback");
    assert_eq!(completed.provider, "route-provider");
    assert_eq!(message_text(&completed), "routed");
    assert_eq!(route_calls.load(Ordering::SeqCst), 1);
    let seen = seen_request
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("route callback saw the request");
    assert_eq!(seen.model["provider"], "virtual-provider");
    assert_eq!(seen.model["id"], "virtual-routed");
    // `CompleteOptions::default()` asks for `low`, but the session under test
    // was built on a `reasoning: false` model, so PIR-BIN-074's
    // construction-time clamp (upstream `createAgentSession`) installed `off`
    // and the route callback sees the level the session would really send.
    assert_eq!(seen.thinking_level.as_deref(), Some("off"));

    let request = route_provider
        .last_request()
        .expect("route provider streamed");
    assert_eq!(
        request.system_prompt.as_deref(),
        Some("virtual system\n\nbase prompt"),
        "the registration's systemPrompt must be prepended like the session path"
    );
    assert_eq!(
        request.max_tokens,
        Some(77),
        "the registration's modelOverride.maxTokens applies without a caller cap"
    );

    // A caller-supplied cap wins over the registration override.
    let _ = service
        .complete(
            &virtual_model,
            plain_context(),
            CompleteOptions {
                max_tokens: Some(11),
                temperature: None,
                signal: None,
            },
        )
        .await
        .expect("second virtual completion");
    assert_eq!(route_provider.last_request().unwrap().max_tokens, Some(11));

    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    drop(ctx);
    drop(session);
}

/// P2-REVIEW2-03: the SAME virtual model must expose the same output cap on
/// the AgentSession prompt path and on `ProviderService::complete`. The old
/// implementation applied `modelOverride.maxTokens` only in `complete`, so a
/// session prompt fell back to the physical model's `max_tokens`.
#[tokio::test(flavor = "current_thread")]
async fn virtual_model_max_tokens_matches_between_session_prompt_and_complete() {
    let temp = TempDir::new("virtual-max-tokens-parity");
    let physical_model = model("session-provider", "session-model");
    let physical = ScriptedProvider::new("session-provider", physical_model, "session");
    let mut registry = ProviderRegistry::default();
    registry.set_provider(boxed_provider(Arc::clone(&physical)));
    let registry = Arc::new(registry);
    let slot = context_slot();
    let definition = json!({
        "id": "virtual-session",
        "provider": "virtual-provider",
        "name": "Virtual Session",
        "physical": { "provider": "session-provider", "id": "session-model" },
        "modelOverride": { "contextWindow": 2048, "maxTokens": 77 }
    });
    let plugin = OnContextPlugin::new("virtual-max-tokens-parity", {
        let slot = Arc::clone(&slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_virtual_model(definition.clone());
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let mut virtual_session_model = model("virtual-provider", "virtual-session");
    virtual_session_model.api = pir::ai::virtual_::VIRTUAL_MODEL_API.to_owned();
    let session = session_with(
        &temp,
        Arc::clone(&registry),
        virtual_session_model,
        vec![plugin],
    );
    let ctx = take_context(&slot);
    let service = ctx.require_providers().expect("providers bound");

    session
        .prompt("hello", Arc::new(|_event| {}))
        .await
        .expect("session prompt");
    let session_request = physical.last_request().expect("session streamed");
    assert_eq!(
        session_request.max_tokens,
        Some(77),
        "the session prompt must use the virtual registration's maxTokens"
    );

    let virtual_model = registry
        .get_model("virtual-provider", "virtual-session")
        .expect("virtual model is listed");
    let completed = service
        .complete(&virtual_model, plain_context(), CompleteOptions::default())
        .await
        .expect("virtual completion");
    assert_eq!(message_text(&completed), "session");
    assert_eq!(
        physical
            .last_request()
            .expect("complete streamed")
            .max_tokens,
        Some(77),
        "complete must agree with the session prompt for the same virtual model"
    );

    // An explicit caller cap still wins on the completion path.
    let _ = service
        .complete(
            &virtual_model,
            plain_context(),
            CompleteOptions {
                max_tokens: Some(11),
                temperature: None,
                signal: None,
            },
        )
        .await
        .expect("capped virtual completion");
    assert_eq!(physical.last_request().unwrap().max_tokens, Some(11));

    drop(ctx);
    drop(session);
}

// ---------------------------------------------------------------------------
// SDK-built session binds the same live provider service
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn sdk_built_session_exposes_the_live_provider_service() {
    let temp = TempDir::new("sdk-provider");
    let (registry, session_model) = base_registry();
    let slot = context_slot();
    let plugin = OnContextPlugin::new("sdk-provider-probe", {
        let slot = Arc::clone(&slot);
        move |ctx| {
            assert!(ctx.providers().is_some(), "provider service bound");
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let agent = AgentBuilder::new()
        .with_registry(Arc::clone(&registry), session_model)
        .thinking_level(ThinkingLevel::Low)
        .with_plugin(plugin)
        .cwd(temp.path())
        .no_session()
        .build()
        .await
        .expect("build SDK agent");
    let ctx = take_context(&slot);
    let service = ctx.require_providers().expect("providers bound");
    let provider =
        ScriptedProvider::new("sdk-native", model("sdk-native", "sdk-model"), "sdk native");
    service.register_provider_impl(boxed_provider(Arc::clone(&provider)));
    assert!(registry.get_model("sdk-native", "sdk-model").is_some());
    drop(ctx);
    drop(agent);
    // Session construction spawns the native `project_trust` fan-out on the
    // current runtime. That task holds the host (and with it the bound
    // services) until it runs, so yield until teardown is observable instead
    // of racing the runnable task.
    for _ in 0..64 {
        if registry.plugin_provider("sdk-native").is_none() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(registry.plugin_provider("sdk-native").is_none());
}

// ---------------------------------------------------------------------------
// Registration with unsupported runtime JSON callbacks is rejected honestly
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn runtime_json_rejects_callback_fields_instead_of_installing_a_stub() {
    let temp = TempDir::new("json-reject");
    let (registry, session_model) = base_registry();
    let slot = context_slot();
    let plugin = OnContextPlugin::new("json-reject-plugin", {
        let slot = Arc::clone(&slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider(
                    "callback-provider",
                    json!({
                        "baseUrl": "http://127.0.0.1:9/v1",
                        "models": [{ "id": "m" }],
                        "streamSimple": true
                    }),
                );
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let session = session_with(&temp, Arc::clone(&registry), session_model, vec![plugin]);
    assert!(
        registry.get_provider_models("callback-provider").is_empty(),
        "a runtime JSON streamSimple definition must not install a stub provider"
    );
    let _ = take_context(&slot);
    drop(session);
}

/// SNAP8 P2-1: native `ProviderService` runtime JSON registrations are
/// session-scoped. Two sessions sharing one registry register the same id;
/// a scoped lookup returns each session's own provider while both are alive,
/// and the finished session's overlay stops resolving.
#[tokio::test(flavor = "current_thread")]
async fn runtime_json_service_registrations_are_session_scoped() {
    let temp = TempDir::new("json-session-scoped");
    let (registry, session_model) = base_registry();

    let parent_slot = context_slot();
    let parent_plugin = OnContextPlugin::new("json-session-parent", {
        let slot = Arc::clone(&parent_slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider(
                    "scoped-json",
                    json_provider_config("parent-json-model", "Parent JSON Model"),
                );
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let parent = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![parent_plugin],
    );
    let parent_ctx = take_context(&parent_slot);
    let parent_key = parent.provider_session_key_for_test();

    let child_slot = context_slot();
    let child_plugin = OnContextPlugin::new("json-session-child", {
        let slot = Arc::clone(&child_slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider(
                    "scoped-json",
                    json_provider_config("child-json-model", "Child JSON Model"),
                );
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let child = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![child_plugin],
    );
    let child_ctx = take_context(&child_slot);
    let child_key = child.provider_session_key_for_test();

    let model_ids = |provider: ProviderBox| -> Vec<String> {
        provider
            .get_models()
            .into_iter()
            .map(|entry| entry.id)
            .collect()
    };
    assert_eq!(
        model_ids(
            registry
                .provider_box_for_session_for_test(&parent_key, "scoped-json")
                .expect("parent scoped provider")
        ),
        vec!["parent-json-model".to_owned()],
        "the parent must resolve its own native registration"
    );
    assert_eq!(
        model_ids(
            registry
                .provider_box_for_session_for_test(&child_key, "scoped-json")
                .expect("child scoped provider")
        ),
        vec!["child-json-model".to_owned()],
        "the child must resolve its own native registration"
    );
    assert_eq!(
        registry
            .get_provider_models("scoped-json")
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec!["child-json-model".to_owned()],
        "the process-shared map still exposes the latest writer"
    );

    parent_ctx
        .require_providers()
        .expect("providers bound")
        .register_provider(
            "scoped-json",
            json_provider_config("parent-2-json-model", "Parent 2 JSON Model"),
        );
    assert_eq!(
        model_ids(
            registry
                .provider_box_for_session_for_test(&parent_key, "scoped-json")
                .expect("parent scoped provider after re-registration")
        ),
        vec!["parent-2-json-model".to_owned()],
        "the parent's re-registration updates its own slot"
    );
    assert_eq!(
        model_ids(
            registry
                .provider_box_for_session_for_test(&child_key, "scoped-json")
                .expect("child scoped provider after parent re-registration")
        ),
        vec!["child-json-model".to_owned()],
        "the parent's re-registration must not leak into the child"
    );

    drop(child_ctx);
    drop(child);
    assert!(
        registry
            .provider_box_for_session_for_test(&child_key, "scoped-json")
            .is_none(),
        "a finished child's native provider must not resolve for its old key"
    );
    assert_eq!(
        model_ids(
            registry
                .provider_box_for_session_for_test(&parent_key, "scoped-json")
                .expect("parent scoped provider after child teardown")
        ),
        vec!["parent-2-json-model".to_owned()],
        "the parent stays on its own registration after child teardown"
    );

    drop(parent_ctx);
    drop(parent);
}

/// SNAP9 P2-S8-3: `ProviderService::complete` resolves its model-existence
/// gate through the OWNING session. Parent and child register the same
/// provider id with different model ids; the process-shared catalog's last
/// writer is the child, so the old global `registry.get_model` pre-check
/// rejected the parent's own model as "unknown model" before the
/// session-scoped stream could run.
#[tokio::test(flavor = "current_thread")]
async fn provider_service_complete_uses_the_session_model_catalog() {
    let temp = TempDir::new("complete-session-model");
    let (registry, session_model) = base_registry();

    let parent_slot = context_slot();
    let parent_provider = ScriptedProvider::new(
        "scoped-complete",
        model("scoped-complete", "parent-model"),
        "parent-complete",
    );
    let parent_plugin = OnContextPlugin::new("complete-session-parent", {
        let provider = boxed_provider(Arc::clone(&parent_provider));
        let slot = Arc::clone(&parent_slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let parent = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![parent_plugin],
    );
    let parent_ctx = take_context(&parent_slot);

    let child_slot = context_slot();
    let child_provider = ScriptedProvider::new(
        "scoped-complete",
        model("scoped-complete", "child-model"),
        "child-complete",
    );
    let child_plugin = OnContextPlugin::new("complete-session-child", {
        let provider = boxed_provider(Arc::clone(&child_provider));
        let slot = Arc::clone(&child_slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let child = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![child_plugin],
    );
    let child_ctx = take_context(&child_slot);

    // The registry-wide catalog is last-writer-wins and only knows the
    // child's model; the old global pre-check would reject the parent's.
    assert!(
        registry
            .get_model("scoped-complete", "parent-model")
            .is_none(),
        "the process-shared catalog must be owned by the child"
    );
    assert!(
        registry
            .get_model("scoped-complete", "child-model")
            .is_some(),
        "the child's model is the shared last writer"
    );

    let parent_service = parent_ctx.require_providers().expect("providers bound");
    let completed = parent_service
        .complete(
            &model("scoped-complete", "parent-model"),
            plain_context(),
            CompleteOptions::default(),
        )
        .await
        .expect("the parent's own model must not be reported as unknown");
    assert_eq!(message_text(&completed), "parent-complete");
    assert_eq!(
        parent_provider.streams(),
        1,
        "the parent completion must dispatch the parent's provider"
    );
    assert_eq!(
        child_provider.streams(),
        0,
        "the parent completion must never reach the child's provider"
    );

    let child_service = child_ctx.require_providers().expect("providers bound");
    let completed = child_service
        .complete(
            &model("scoped-complete", "child-model"),
            plain_context(),
            CompleteOptions::default(),
        )
        .await
        .expect("the child's own model must not be reported as unknown");
    assert_eq!(message_text(&completed), "child-complete");
    assert_eq!(child_provider.streams(), 1);

    // The gate stays session-scoped in the other direction: a model that only
    // the sibling registered is unknown to this session.
    let error = parent_service
        .complete(
            &model("scoped-complete", "child-model"),
            plain_context(),
            CompleteOptions::default(),
        )
        .await
        .expect_err("a sibling-only model must fail closed");
    assert!(
        error.to_string().contains("unknown model"),
        "expected the completion boundary error, got {error}"
    );

    drop(parent_ctx);
    drop(child_ctx);
    drop(child);
    drop(parent);
}

/// SNAP10 P2-S8-2: the TUI's provider-list refresh path calls
/// `AgentSession::refresh_provider_models` → `ProviderRegistry::refresh_for_session`.
/// Parent and child register the same provider id; the process-shared
/// `plugin_owned` winner is the child, so the old session-less `refresh` would
/// refresh the child's provider for a parent request. The session-scoped path
/// must refresh the OWNER's provider, and the legacy entry point must keep the
/// pre-SNAP8 last-writer behavior.
#[tokio::test(flavor = "current_thread")]
async fn provider_refresh_uses_the_session_provider() {
    let temp = TempDir::new("refresh-session-provider");
    let (registry, session_model) = base_registry();

    let parent_provider = ScriptedProvider::new(
        "scoped-refresh",
        model("scoped-refresh", "parent-model"),
        "parent-refresh",
    );
    let parent_slot = context_slot();
    let parent_plugin = OnContextPlugin::new("refresh-session-parent", {
        let provider = boxed_provider(Arc::clone(&parent_provider));
        let slot = Arc::clone(&parent_slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let parent = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![parent_plugin],
    );
    let parent_ctx = take_context(&parent_slot);

    let child_provider = ScriptedProvider::new(
        "scoped-refresh",
        model("scoped-refresh", "child-model"),
        "child-refresh",
    );
    let child_slot = context_slot();
    let child_plugin = OnContextPlugin::new("refresh-session-child", {
        let provider = boxed_provider(Arc::clone(&child_provider));
        let slot = Arc::clone(&child_slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let child = session_with(
        &temp,
        Arc::clone(&registry),
        session_model,
        vec![child_plugin],
    );
    let child_ctx = take_context(&child_slot);

    let only_scoped = || ModelsRefreshOptions {
        providers: Some(vec!["scoped-refresh".to_owned()]),
        allow_network: false,
        ..ModelsRefreshOptions::default()
    };

    // Parent refresh: the session-scoped provider lookup must pick the
    // parent's typed entry even though the shared winner is the child.
    let outcome = parent.refresh_provider_models(only_scoped()).await;
    assert!(
        outcome.errors.is_empty(),
        "parent refresh errors: {:?}",
        outcome.errors
    );
    assert_eq!(
        parent_provider.refreshes(),
        1,
        "the parent's provider must be refreshed"
    );
    assert_eq!(
        child_provider.refreshes(),
        0,
        "the parent's refresh must never reach the child's provider"
    );
    let parent_pairs = parent.provider_model_pairs();
    assert!(
        parent_pairs.contains(&("scoped-refresh".to_owned(), "parent-model".to_owned())),
        "the parent's model list must be its own overlay: {parent_pairs:?}"
    );
    assert!(
        !parent_pairs.contains(&("scoped-refresh".to_owned(), "child-model".to_owned())),
        "the parent's model list must not expose the child's model: {parent_pairs:?}"
    );

    // Child refresh resolves the child's own entry.
    let outcome = child.refresh_provider_models(only_scoped()).await;
    assert!(
        outcome.errors.is_empty(),
        "child refresh errors: {:?}",
        outcome.errors
    );
    assert_eq!(child_provider.refreshes(), 1);
    assert_eq!(parent_provider.refreshes(), 1);

    // The legacy session-less entry point keeps its pre-SNAP8 behavior: the
    // process-shared winner (the child) is the one refreshed.
    let outcome = registry.refresh(only_scoped()).await;
    assert!(
        outcome.errors.is_empty(),
        "legacy refresh errors: {:?}",
        outcome.errors
    );
    assert_eq!(
        child_provider.refreshes(),
        2,
        "the legacy path must keep the shared last-writer semantics"
    );
    assert_eq!(parent_provider.refreshes(), 1);

    drop(parent_ctx);
    drop(child_ctx);
    drop(child);
    drop(parent);
}

/// SNAP10 P2-S8-2 no-overlay regression: a session without any provider
/// overlay over a static refreshable provider resolves the static provider for
/// both the session-scoped and legacy refresh entry points.
#[tokio::test(flavor = "current_thread")]
async fn provider_refresh_without_overlay_keeps_the_static_provider() {
    let temp = TempDir::new("refresh-static-provider");
    let static_model = model("static-refresh", "static-model");
    let static_provider =
        ScriptedProvider::new("static-refresh", static_model.clone(), "static-refresh");
    let mut base = ProviderRegistry::default();
    base.set_provider(boxed_provider(Arc::clone(&static_provider)));
    let registry = Arc::new(base);
    let session = session_with(
        &temp,
        Arc::clone(&registry),
        static_model.clone(),
        Vec::new(),
    );

    let options = || ModelsRefreshOptions {
        providers: Some(vec!["static-refresh".to_owned()]),
        allow_network: false,
        ..ModelsRefreshOptions::default()
    };
    let outcome = session.refresh_provider_models(options()).await;
    assert!(
        outcome.errors.is_empty(),
        "static refresh errors: {:?}",
        outcome.errors
    );
    assert_eq!(
        static_provider.refreshes(),
        1,
        "a session without an overlay refreshes the static provider"
    );
    let key = session.provider_session_key_for_test();
    assert!(
        registry
            .provider_box_for_session_for_test(&key, "static-refresh")
            .is_some(),
        "the static provider stays session-visible"
    );

    let outcome = registry.refresh(options()).await;
    assert!(outcome.errors.is_empty());
    assert_eq!(
        static_provider.refreshes(),
        2,
        "the legacy refresh still sees the same static provider"
    );
    drop(session);
}

// ---------------------------------------------------------------------------
// SNAP11 P2-S8-4 residual: branch summary and cache warmer are session-scoped
// ---------------------------------------------------------------------------

/// SNAP11 P2-S8-4: `/tree` branch summarization must resolve the provider (and
/// credential) through the owning session. Parent and child register the same
/// provider id with different model ids; the process-shared table is
/// last-writer-wins, so the pre-SNAP11 session-less completion would summarize
/// the parent's branch with the child's provider.
#[tokio::test(flavor = "current_thread")]
async fn branch_summary_uses_the_session_provider_catalog() {
    let temp = TempDir::new("branch-session-provider");
    let (registry, _session_model) = base_registry();

    let parent_provider = ScriptedProvider::new(
        "scoped-branch",
        model("scoped-branch", "parent-model"),
        "parent-branch-summary",
    );
    let parent_slot = context_slot();
    let parent_plugin = OnContextPlugin::new("branch-session-parent", {
        let provider = boxed_provider(Arc::clone(&parent_provider));
        let slot = Arc::clone(&parent_slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let (parent_session, parent_ids) = branched_session();
    let parent = session_with_manager(
        &temp,
        Arc::clone(&registry),
        model("scoped-branch", "parent-model"),
        parent_session,
        vec![parent_plugin],
    );
    let parent_ctx = take_context(&parent_slot);

    let child_provider = ScriptedProvider::new(
        "scoped-branch",
        model("scoped-branch", "child-model"),
        "child-branch-summary",
    );
    let child_slot = context_slot();
    let child_plugin = OnContextPlugin::new("branch-session-child", {
        let provider = boxed_provider(Arc::clone(&child_provider));
        let slot = Arc::clone(&child_slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let (child_session, child_ids) = branched_session();
    let child = session_with_manager(
        &temp,
        Arc::clone(&registry),
        model("scoped-branch", "child-model"),
        child_session,
        vec![child_plugin],
    );
    let child_ctx = take_context(&child_slot);

    // The process-shared catalog only knows the child's model.
    assert!(
        registry
            .get_model("scoped-branch", "parent-model")
            .is_none(),
        "the process-shared catalog must be owned by the child"
    );
    assert!(
        registry.get_model("scoped-branch", "child-model").is_some(),
        "the child's model is the shared last writer"
    );

    let navigation = parent
        .navigate_tree(
            &parent_ids[0],
            true,
            None,
            false,
            // Zero reserve keeps the fixture's 4096-token window above the
            // serialized branch size, so the chunked runner emits exactly one
            // summarizer call per session (the provider attribution under test).
            0,
        )
        .await
        .expect("storage configured")
        .expect("parent navigation");
    assert!(navigation.summary_entry_id.is_some());
    assert_eq!(
        parent_provider.streams(),
        1,
        "the parent branch summary must dispatch the parent's provider"
    );
    assert_eq!(
        child_provider.streams(),
        0,
        "the parent branch summary must never reach the child's provider"
    );
    assert_eq!(
        parent_provider
            .last_request()
            .expect("parent summary request")
            .model_id,
        "parent-model",
        "the summary request must carry the parent's routed model"
    );

    // Make the process-shared last writer the parent again: a session-less
    // child summary would now resolve the parent's provider.
    let parent_provider_2 = ScriptedProvider::new(
        "scoped-branch",
        model("scoped-branch", "parent-model"),
        "parent-branch-summary-2",
    );
    parent_ctx
        .require_providers()
        .expect("providers bound")
        .register_provider_impl(boxed_provider(Arc::clone(&parent_provider_2)));

    let navigation = child
        .navigate_tree(
            &child_ids[0],
            true,
            None,
            false,
            // Zero reserve keeps the fixture's 4096-token window above the
            // serialized branch size, so the chunked runner emits exactly one
            // summarizer call per session (the provider attribution under test).
            0,
        )
        .await
        .expect("storage configured")
        .expect("child navigation");
    assert!(navigation.summary_entry_id.is_some());
    assert_eq!(
        child_provider.streams(),
        1,
        "the child branch summary must dispatch the child's provider even when the parent owns the shared table"
    );
    assert_eq!(
        parent_provider_2.streams(),
        0,
        "the child branch summary must never reach the parent's provider"
    );
    assert_eq!(
        child_provider
            .last_request()
            .expect("child summary request")
            .model_id,
        "child-model"
    );

    drop(parent_ctx);
    drop(child_ctx);
    drop(child);
    drop(parent);
}

/// SNAP11 P2-S8-4 no-overlay regression: a branch summary with no session
/// overlay keeps resolving the static provider (the pre-SNAP11 behavior).
#[tokio::test(flavor = "current_thread")]
async fn branch_summary_without_overlay_keeps_the_static_provider() {
    let temp = TempDir::new("branch-static-provider");
    let static_model = model("static-branch", "static-model");
    let static_provider = ScriptedProvider::new(
        "static-branch",
        static_model.clone(),
        "static-branch-summary",
    );
    let mut base = ProviderRegistry::default();
    base.set_provider(boxed_provider(Arc::clone(&static_provider)));
    let registry = Arc::new(base);
    let (session, ids) = branched_session();
    let agent = session_with_manager(&temp, registry, static_model, session, Vec::new());

    let navigation = agent
        .navigate_tree(
            &ids[0], true, None, false,
            // Zero reserve keeps the fixture's 4096-token window above the
            // serialized branch size, so the chunked runner emits exactly one
            // summarizer call per session (the provider attribution under test).
            0,
        )
        .await
        .expect("storage configured")
        .expect("navigation");
    assert!(navigation.summary_entry_id.is_some());
    assert_eq!(
        static_provider.streams(),
        1,
        "the static provider is still used when the session has no overlay"
    );
    assert_eq!(
        static_provider
            .last_request()
            .expect("summary request")
            .model_id,
        "static-model"
    );
    drop(agent);
}

/// SNAP11 P2-S8-4: cache warming is a detached request. It snapshots the
/// session's provider identity (a `Weak` key) and resolves through that
/// overlay while the session is live, so a parent warm cannot replay through a
/// live child's same-id provider. Re-registering the parent after the child
/// flips the process-shared last writer, proving the child warm is scoped too.
#[tokio::test(flavor = "current_thread")]
async fn cache_warmer_uses_the_session_provider_catalog() {
    let temp = TempDir::new("warm-session-provider");
    let (registry, _session_model) = base_registry();

    let parent_provider = ScriptedProvider::new_with_cache_write(
        "scoped-warm",
        model("scoped-warm", "parent-model"),
        "parent-warm",
        100,
    );
    let parent_slot = context_slot();
    let parent_plugin = OnContextPlugin::new("warm-session-parent", {
        let provider = boxed_provider(Arc::clone(&parent_provider));
        let slot = Arc::clone(&parent_slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let parent = session_with(
        &temp,
        Arc::clone(&registry),
        model("scoped-warm", "parent-model"),
        vec![parent_plugin],
    );
    parent.set_cache_warming(Some(CacheWarmingSettings::new(CacheWarmingMode::Streaming)));
    let parent_ctx = take_context(&parent_slot);

    let child_provider = ScriptedProvider::new_with_cache_write(
        "scoped-warm",
        model("scoped-warm", "child-model"),
        "child-warm",
        100,
    );
    let child_slot = context_slot();
    let child_plugin = OnContextPlugin::new("warm-session-child", {
        let provider = boxed_provider(Arc::clone(&child_provider));
        let slot = Arc::clone(&child_slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let child = session_with(
        &temp,
        Arc::clone(&registry),
        model("scoped-warm", "child-model"),
        vec![child_plugin],
    );
    child.set_cache_warming(Some(CacheWarmingSettings::new(CacheWarmingMode::Streaming)));
    let child_ctx = take_context(&child_slot);

    assert!(
        registry.get_model("scoped-warm", "parent-model").is_none(),
        "the process-shared catalog must be owned by the child"
    );
    assert!(
        registry.get_model("scoped-warm", "child-model").is_some(),
        "the child's model is the shared last writer"
    );

    let terminal = parent
        .prompt("warm the parent", Arc::new(|_| {}))
        .await
        .expect("parent prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    wait_for_streams(&parent_provider, 2).await;
    assert_eq!(
        parent_provider.streams(),
        2,
        "the parent run plus its warm request both use the parent provider"
    );
    assert_eq!(
        child_provider.streams(),
        0,
        "the parent warm must never reach the child's provider"
    );
    let warm_request = parent_provider.last_request().expect("warm request");
    assert_eq!(warm_request.max_tokens, Some(1));
    assert_eq!(warm_request.model_id, "parent-model");

    // Flip the process-shared last writer to the parent; the child's warm
    // must still resolve its own provider.
    let parent_provider_2 = ScriptedProvider::new_with_cache_write(
        "scoped-warm",
        model("scoped-warm", "parent-model"),
        "parent-warm-2",
        100,
    );
    parent_ctx
        .require_providers()
        .expect("providers bound")
        .register_provider_impl(boxed_provider(Arc::clone(&parent_provider_2)));

    let terminal = child
        .prompt("warm the child", Arc::new(|_| {}))
        .await
        .expect("child prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    wait_for_streams(&child_provider, 2).await;
    assert_eq!(
        child_provider.streams(),
        2,
        "the child run plus its warm request both use the child provider"
    );
    assert_eq!(
        parent_provider_2.streams(),
        0,
        "the child warm must never reach the parent's shared entry"
    );
    let warm_request = child_provider.last_request().expect("warm request");
    assert_eq!(warm_request.max_tokens, Some(1));
    assert_eq!(warm_request.model_id, "child-model");

    drop(parent_ctx);
    drop(child_ctx);
    drop(child);
    drop(parent);
}

/// SNAP11 P2-S8-4 fallback: the warm task's `ProviderSessionKey` is a `Weak`
/// snapshot. Once the session is gone the warmer must resolve through the
/// legacy global path (the static provider), never through a dead overlay; the
/// session-less API keeps the same static behavior.
#[tokio::test(flavor = "current_thread")]
async fn cache_warmer_falls_back_to_the_global_provider_after_session_end() {
    let temp = TempDir::new("warm-session-fallback");
    let static_model = model("scoped-fallback", "static-model");
    let static_provider = ScriptedProvider::new_with_cache_write(
        "scoped-fallback",
        static_model.clone(),
        "static-warm",
        100,
    );
    let mut base = ProviderRegistry::default();
    base.set_provider(boxed_provider(Arc::clone(&static_provider)));
    let registry = Arc::new(base);

    let session_model = model("scoped-fallback", "session-model");
    let session_provider = ScriptedProvider::new_with_cache_write(
        "scoped-fallback",
        session_model.clone(),
        "session-warm",
        100,
    );
    let slot = context_slot();
    let plugin = OnContextPlugin::new("warm-session-fallback", {
        let provider = boxed_provider(Arc::clone(&session_provider));
        let slot = Arc::clone(&slot);
        move |ctx| {
            ctx.require_providers()
                .expect("providers bound")
                .register_provider_impl(Arc::clone(&provider));
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        }
    });
    let session = session_with(
        &temp,
        Arc::clone(&registry),
        session_model.clone(),
        vec![plugin],
    );
    let ctx = take_context(&slot);
    let key = session.provider_session_key_for_test();

    // Live key: the session's own provider wins.
    let message = warm_once_with_hooks_owned_for_session(
        &registry,
        Some(&key),
        &session_model,
        (*plain_context()).clone(),
        None,
        None,
        Default::default(),
    )
    .await
    .expect("live session warm");
    assert_eq!(message_text(&message), "session-warm");
    assert_eq!(session_provider.streams(), 1);
    assert_eq!(static_provider.streams(), 0);

    drop(ctx);
    drop(session);
    // The static provider under the same id is the liveness probe: a dead key
    // resolves `None` before falling through, while a live key with no entry
    // would resolve the static provider.
    let mut key_is_dead = false;
    for _ in 0..100 {
        if registry
            .provider_box_for_session_for_test(&key, "scoped-fallback")
            .is_none()
        {
            key_is_dead = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        key_is_dead,
        "the cache warmer's Weak session snapshot must not keep the session alive"
    );

    // The dead snapshot falls back to the pre-SNAP11 global path.
    let message = warm_once_with_hooks_owned_for_session(
        &registry,
        Some(&key),
        &static_model,
        (*plain_context()).clone(),
        None,
        None,
        Default::default(),
    )
    .await
    .expect("global fallback warm");
    assert_eq!(message_text(&message), "static-warm");
    assert_eq!(
        static_provider.streams(),
        1,
        "a warm whose session ended must use the global registry"
    );
    assert_eq!(
        session_provider.streams(),
        1,
        "the dead session's provider must not receive the warm"
    );

    // The legacy session-less entry point keeps exactly the same behavior.
    let message = warm_once_with_hooks_owned(
        &registry,
        &static_model,
        (*plain_context()).clone(),
        None,
        None,
        Default::default(),
    )
    .await
    .expect("session-less warm");
    assert_eq!(message_text(&message), "static-warm");
    assert_eq!(static_provider.streams(), 2);
}
