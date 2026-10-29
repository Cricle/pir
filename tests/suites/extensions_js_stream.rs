//! End-to-end tests for the JS-callback extension APIs added in the Q7 slice:
//!
//! - `pi.registerProvider({streamSimple})`: the JS callback produces the
//!   assistant event stream per request through the reserved `__pir_stream:`
//!   actor channel (`src/ai/providers/js_stream.rs`), and a throwing callback
//!   fails closed as a typed error event.
//! - `pi.registerVirtualModel({route})`: the per-request `route(request)`
//!   callback routes through the reserved `__pir_route:` channel, and a
//!   throwing callback falls back to the static route.
//!
//! Fixtures are generated into per-test temp directories (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig, SharedEventStreamFn};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::extensions::{ExtensionConfig, ExtensionHandle};

// ---------------------------------------------------------------------------
// Fixtures and fakes
// ---------------------------------------------------------------------------

fn js_config(cwd: &std::path::Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "test".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn seed_and_load(handle: &ExtensionHandle, dir: &std::path::Path, source: &str) {
    let path = dir.join("ext.ts");
    std::fs::write(&path, source).unwrap();
    let result = handle.load(std::slice::from_ref(&path)).await.unwrap();
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
}

/// Per-test temp directory removed at the end of each test body.
fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-js-stream-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn js_model(id: &str) -> Model {
    js_model_for_provider("jsprov", id)
}

fn js_model_for_provider(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: "JS model".to_string(),
        api: "openai-chat".to_string(),
        provider: provider.to_string(),
        base_url: "http://localhost".to_string(),
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
        context_window: 1000,
        max_tokens: 100,
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

/// Physical fake serving `fake-model` and `fake-model-2` under provider
/// `fake` (the route-callback tests' routing targets).
struct FakeProvider {
    calls: AtomicUsize,
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        let mut model = fake_physical("fake-model");
        let second = fake_physical("fake-model-2");
        model.input = vec!["text".to_string()];
        vec![model, second]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let _ = self.calls.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "fake-api".to_string(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                content: vec![ContentBlock::Text(TextContent {
                    text: format!("answered by {}", model.id),
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

fn fake_physical(id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: format!("Fake {id}"),
        api: "fake-api".to_string(),
        provider: "fake".to_string(),
        base_url: "http://localhost".to_string(),
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

/// A virtual catalog entry model (api `pi-virtual`).
fn virtual_model(id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: format!("Virtual {id}"),
        api: pir::ai::virtual_::VIRTUAL_MODEL_API.to_string(),
        provider: "fake".to_string(),
        base_url: String::new(),
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

// ---------------------------------------------------------------------------
// 1. streamSimple: events stream through the session, failures fail closed
// ---------------------------------------------------------------------------

const STREAM_EXTENSION_TS: &str = r#"
export default function (pi) {
  const usage = {
    input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2,
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
  };
  pi.registerProvider("jsprov", {
    baseUrl: "http://localhost",
    api: "openai-chat",
    models: [{ id: "js-model", name: "JS model", contextWindow: 1000, maxTokens: 100, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
    streamSimple: (model, context, options) => {
      if (!model || model.api !== "openai-chat") throw new Error("model argument missing");
      if (!Array.isArray(context.messages) || context.messages.length === 0) throw new Error("context messages missing");
      return [
        { type: "text_delta", content_index: 0, delta: "hello from js" },
        {
          type: "done", reason: "stop",
          message: {
            role: "assistant",
            content: [{ type: "text", text: "hello from js" }],
            api: model.api, provider: model.provider, model: model.id,
            usage, stopReason: "stop", timestamp: 1,
          },
        },
      ];
    },
  });
  pi.registerProvider("jsboom", {
    baseUrl: "http://localhost",
    api: "openai-chat",
    models: [{ id: "boom-model", name: "Boom model", contextWindow: 1000, maxTokens: 100, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
    streamSimple: () => { throw new Error("boom"); },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn js_stream_provider_streams_events_through_the_session() {
    let dir = temp_dir("events");
    let handle = ExtensionHandle::start(js_config(&dir)).unwrap();
    seed_and_load(&handle, &dir, STREAM_EXTENSION_TS).await;

    let registry = Arc::new(ProviderRegistry::default());
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry.clone(),
        model: js_model("js-model"),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(dir.clone()),
        event_stream: None,
        extensions: Some(handle),
        tool_selection: None,
    });

    let terminal = agent.prompt("hi", Arc::new(|_| {})).await.unwrap();
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    assert_eq!(terminal.provider, "jsprov");
    assert_eq!(terminal.model, "js-model");
    let text = terminal
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "hello from js");

    // The provider is registered in the shared registry overlay (catalog).
    assert!(registry
        .get_provider_models("jsprov")
        .iter()
        .any(|model| model.id == "js-model"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn js_stream_provider_callback_failure_fails_closed() {
    let dir = temp_dir("fail");
    let handle = ExtensionHandle::start(js_config(&dir)).unwrap();
    seed_and_load(&handle, &dir, STREAM_EXTENSION_TS).await;

    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        // The throwing provider (its streamSimple callback always throws).
        model: js_model_for_provider("jsboom", "boom-model"),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(dir.clone()),
        event_stream: None,
        extensions: Some(handle),
        tool_selection: None,
    });
    // One failing attempt is terminal: no retry loop over the throwing hook.
    agent.set_auto_retry_enabled(false);

    let terminal = agent.prompt("hi", Arc::new(|_| {})).await.unwrap();
    assert_eq!(terminal.stop_reason, StopReason::Error);
    let error = terminal.error_message.as_deref().unwrap_or_default();
    assert!(
        error.contains("boom"),
        "typed error event carries the JS error: {error}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2. virtual-model route(request): per-request routing + static fallback
// ---------------------------------------------------------------------------

const ROUTE_EXTENSION_TS: &str = r#"
export default function (pi) {
  pi.registerVirtualModel({
    id: "router-ok",
    physical: { provider: "fake", id: "fake-model" },
    route: (request) => {
      if (!request.model || request.model.api !== "pi-virtual") {
        throw new Error("route request model missing");
      }
      if (!Array.isArray(request.messages) || request.messages.length === 0) {
        throw new Error("route request messages missing");
      }
      return { model: { provider: "fake", id: "fake-model-2" } };
    },
  });
  pi.registerVirtualModel({
    id: "router-fail",
    physical: { provider: "fake", id: "fake-model" },
    route: () => { throw new Error("router exploded"); },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn js_virtual_route_callback_routes_and_falls_back_to_static() {
    let dir = temp_dir("route");
    let handle = ExtensionHandle::start(js_config(&dir)).unwrap();
    seed_and_load(&handle, &dir, ROUTE_EXTENSION_TS).await;

    let mut inner = ProviderRegistry::default();
    inner.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
    }));
    let registry = Arc::new(inner);

    // Record the physical model each request actually carries after routing.
    let routed: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let routed_log = Arc::clone(&routed);
    let stream_registry = Arc::clone(&registry);
    let event_stream: SharedEventStreamFn = Arc::new(move |request| {
        let model = request.model.expect("routed request carries a model");
        routed_log
            .lock()
            .unwrap()
            .push((model.provider.clone(), model.id.clone()));
        let registry = Arc::clone(&stream_registry);
        Box::pin(async move {
            // `ProviderRequest.context` is shared (`Arc<Context>`); the
            // provider trait takes an owned context.
            registry.stream_unchecked(
                model,
                pir::agent::agent_loop::context_into_owned(request.context),
                StreamOptions::default(),
            )
        })
    });

    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry.clone(),
        model: virtual_model("router-ok"),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(dir.clone()),
        event_stream: Some(event_stream),
        extensions: Some(handle),
        tool_selection: None,
    });

    // Success: the route callback redirects to the other physical model.
    let terminal = agent.prompt("first", Arc::new(|_| {})).await.unwrap();
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    assert_eq!(
        routed.lock().unwrap().clone(),
        vec![("fake".to_string(), "fake-model-2".to_string())],
        "route(request) callback picked the physical model"
    );
    // The recorded response keeps the PHYSICAL identity the routing selected
    // (upstream `agent-session.ts:614-637`; P10) — the virtual selection itself
    // is the branch's `model_change`, which a resume restores.
    assert_eq!(terminal.model, "fake-model-2");

    // Failure: a throwing route callback falls back to the static route.
    agent.set_model(virtual_model("router-fail")).await.unwrap();
    let terminal = agent.prompt("second", Arc::new(|_| {})).await.unwrap();
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    assert_eq!(
        routed.lock().unwrap().clone(),
        vec![
            ("fake".to_string(), "fake-model-2".to_string()),
            ("fake".to_string(), "fake-model".to_string()),
        ],
        "static route used after the callback threw"
    );
    // The static route's physical model, likewise recorded as the response's
    // identity (P10).
    assert_eq!(terminal.model, "fake-model");

    let _ = std::fs::remove_dir_all(&dir);
}
