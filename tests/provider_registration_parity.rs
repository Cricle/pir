//! Provider-registration long-tail parity (upstream registerProvider):
//!
//! 1. `oauth` config segment (#12): the JSON definition carries the oauth
//!    markers (`name`/`isSubscription` plus `login/refreshToken/getApiKey:
//!    true`), and the JS login/refresh callbacks execute through the detached
//!    start/poll bridge — notify events reach the login `AuthInteraction`,
//!    prompts ride the dialog channel, credentials persist, and `to_auth`
//!    derives the API key from the credential's `apiKey` extra field.
//! 2. `refreshModels` hook (#12): the `provider:refreshModels` synthetic
//!    event reaches the registered JS callback and its result rides the
//!    event outcome (the session consumes the same shape at registration).
//! 3. baseUrl-only override (#12): a model-less registration with `baseUrl`
//!    overrides the URL of an existing provider's models (upstream
//!    `applyExtension`), and the dynamic overlay wins over the static entry.
//! 4. `api: "google-generative-ai"` (#22): dynamic registration routes to the
//!    dedicated Google adapter (`build_provider_from_models`), not the
//!    OpenAI-compatible fallback.
//! 5. `ctx.waitForIdle()` (#13): resolves immediately when idle and blocks
//!    until the session-pushed streaming flag clears.
//! 6. autocomplete factory chain (#30): each factory wraps the previous
//!    provider; the last registration is outermost (upstream
//!    `AutocompleteProviderFactory`).
//! 7. mid-session definition gate: a registration landing after the first
//!    registry build that touches NO JS tool name — a `session_start` handler
//!    calling `pi.registerProvider`, or a late-loaded extension registering a
//!    provider/command/flag/shortcut/virtual model — is still published by
//!    the emit-tail registry refresh (one fingerprint per definition class
//!    `JsExtensionsPlugin::register` imports), and a refresh with no
//!    definition change does not rebuild at all.
//!
//! Fixture pattern follows tests/suites/extensions_events.rs (real QuickJS
//! runtime + generated per-test extension sources; no tracked .ts files).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::{
    AuthEvent, AuthInteraction, AuthPrompt, LoginOptions, OAuthCredential, OAuthFlow,
};
use pir::ai::error::AiResult;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::providers::js_stream::JsOAuthFlow;
use pir::ai::providers::{DynamicProviderConfig, RegisterDynamicProviderShared};
use pir::ai::types::*;
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::plugins::{PluginHost, PluginRegistry};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-provider-registration-{label}-{}-{}",
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("prov-model".to_string()),
        session_id: Some("prov-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn load_runtime(dir: &TempDir, source: &str) -> ExtensionHandle {
    let path = dir.0.join("ext.ts");
    std::fs::write(&path, source).expect("write fixture");
    let runtime = ExtensionHandle::start(test_config_at(&dir.0)).expect("runtime");
    let result = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
    runtime
}

async fn probe(runtime: &ExtensionHandle, code: &str) -> Value {
    runtime
        .eval_json(&format!("JSON.stringify({code})"))
        .await
        .expect("probe eval")
}

/// A minimal chat model used by the registration fixtures.
fn fixture_model(id: &str, base_url: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "openai-chat".to_string(),
        provider: String::new(),
        base_url: base_url.to_string(),
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

/// A static provider with a fixed model list (stands in for a built-in
/// catalog provider in the baseUrl-override test).
struct StaticProvider {
    id: &'static str,
    models: Vec<Model>,
}

impl Provider for StaticProvider {
    fn id(&self) -> &str {
        self.id
    }

    fn name(&self) -> &str {
        self.id
    }

    fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> pir::ai::event_stream::AssistantMessageEventStream {
        let output = pir::ai::event_stream::AssistantMessageEventStream::new();
        output.end(None);
        output
    }

    fn api_key_auth(&self) -> Vec<pir::ai::auth::ApiKeyAuth> {
        Vec::new()
    }
}

/// An AgentSession wired with a JS extension runtime (exercises
/// sync_plugin_providers through the prompt path; the model streaming itself
/// is irrelevant to these assertions).
fn session_with_extensions(
    registry: Arc<ProviderRegistry>,
    model: Model,
    runtime: ExtensionHandle,
    cwd: &Path,
) -> Arc<AgentSession> {
    Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model,
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: None,
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: Some(runtime),
        tool_selection: None,
    }))
}

/// A login interaction that records notify events (headless prompt stub).
#[derive(Default)]
struct RecordingInteraction {
    events: Mutex<Vec<String>>,
}

impl AuthInteraction for RecordingInteraction {
    fn prompt<'a>(
        &'a self,
        _prompt: AuthPrompt,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<String>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn notify(&self, event: AuthEvent) {
        let label = match event {
            AuthEvent::Info { message, .. } => format!("info:{message}"),
            AuthEvent::AuthUrl { url, .. } => format!("auth_url:{url}"),
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => format!("device_code:{user_code}@{verification_uri}"),
            AuthEvent::Progress { message } => format!("progress:{message}"),
        };
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(label);
    }
}

impl RecordingInteraction {
    fn labels(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

// ---------------------------------------------------------------------------
// 1. oauth config segment (#12)
// ---------------------------------------------------------------------------

const OAUTH_EXTENSION_TS: &str = r#"
export default function (pi) {
  pi.registerProvider("oauprov", {
    baseUrl: "http://localhost",
    api: "openai-chat",
    models: [{ id: "oau-model", name: "Oau model", contextWindow: 1000, maxTokens: 100, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
    oauth: {
      name: "Oau Provider",
      isSubscription: true,
      login: async (callbacks) => {
        callbacks.onAuth({ url: "https://auth.example/open", instructions: "open me" });
        callbacks.onProgress("step-1");
        const pasted = await callbacks.onPrompt({ message: "Paste the code" });
        // Headless host: the dialog channel answers null; a TUI host would
        // return the typed string.
        globalThis.__oauPromptReply = pasted;
        if (pasted != null && pasted !== "the-code") {
          throw new Error("unexpected prompt reply: " + pasted);
        }
        return { refresh: "r2", access: "a2", expires: 9 };
      },
      refreshToken: async (credentials, signal) => {
        globalThis.__oauRefreshSeen = credentials;
        globalThis.__oauRefreshSignalAborted = signal ? signal.aborted : null;
        return { refresh: "r3", access: "a3", expires: 456 };
      },
      getApiKey: (credentials) => "key-" + credentials.access,
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn oauth_segment_carries_markers_and_login_bridge_round_trips() {
    let temp = TempDir::new("oauth-markers");
    let runtime = load_runtime(&temp, OAUTH_EXTENSION_TS).await;

    // The JSON definition carries the oauth segment with boolean markers (the
    // callbacks stay in JS state, upstream ProviderConfig.oauth fields ride).
    let defs = runtime.provider_defs();
    let def = defs
        .iter()
        .find(|def| def.get("id").and_then(Value::as_str) == Some("oauprov"))
        .expect("provider definition recorded");
    let oauth = def
        .pointer("/config/oauth")
        .expect("oauth segment present in the JSON definition");
    assert_eq!(oauth.get("name"), Some(&json!("Oau Provider")));
    assert_eq!(oauth.get("isSubscription"), Some(&json!(true)));
    assert_eq!(oauth.get("login"), Some(&json!(true)));
    assert_eq!(oauth.get("refreshToken"), Some(&json!(true)));
    assert_eq!(oauth.get("getApiKey"), Some(&json!(true)));

    // Login: notify events reach the AuthInteraction; the prompt rides the
    // dialog channel; the credential and derived API key persist.
    let flow = JsOAuthFlow::new("oauprov", "Oau Provider", true, runtime.clone());
    let interaction = RecordingInteraction::default();
    let credential = flow.login(&interaction, &LoginOptions::default()).await;
    let credential = match credential {
        Ok(credential) => credential,
        Err(error) => panic!("login failed: {error}"),
    };
    assert_eq!(credential.access, "a2");
    assert_eq!(credential.refresh, "r2");
    assert_eq!(
        credential.extra_str("apiKey"),
        Some("key-a2"),
        "the derived API key is stored in the credential extra field"
    );
    let labels = interaction.labels();
    assert!(
        labels
            .iter()
            .any(|label| label == "auth_url:https://auth.example/open"),
        "onAuth event reached the login interaction: {labels:?}"
    );
    assert!(
        labels.iter().any(|label| label == "progress:step-1"),
        "onProgress event reached the login interaction: {labels:?}"
    );

    // Refresh: the stored credential round-trips to refreshToken and the
    // refreshed credential carries the derived API key; to_auth projects it.
    let stored = OAuthCredential::new("a1", "r1", 42);
    let refreshed = flow.refresh(&stored).await.expect("refresh round trip");
    assert_eq!(refreshed.access, "a3");
    assert_eq!(
        refreshed.extra_str("apiKey"),
        Some("key-a3"),
        "refresh derives and stores the API key"
    );
    let seen = runtime
        .eval_json("JSON.stringify(globalThis.__oauRefreshSeen)")
        .await
        .expect("probe");
    assert_eq!(seen, json!({"refresh":"r1","access":"a1","expires":42}));
    let auth = flow.to_auth(&refreshed).expect("to_auth");
    assert_eq!(auth.api_key.as_deref(), Some("key-a3"));
}

// ---------------------------------------------------------------------------
// 2. refreshModels hook (#12)
// ---------------------------------------------------------------------------

const REFRESH_EXTENSION_TS: &str = r#"
export default function (pi) {
  pi.registerProvider("dirprov", {
    baseUrl: "http://localhost",
    api: "openai-chat",
    models: [{ id: "stale-model", name: "Stale", contextWindow: 1, maxTokens: 1, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
    refreshModels: async (context) => {
      if (typeof context.publish !== "function") throw new Error("missing context.publish");
      return [
        { id: "fresh-a", name: "Fresh A", contextWindow: 2000, maxTokens: 200, reasoning: true, input: ["text", "image"], cost: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 } },
        { id: "fresh-b", name: "Fresh B", contextWindow: 3000, maxTokens: 300, reasoning: false, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } },
      ];
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn refresh_models_hook_returns_models_through_the_synthetic_event() {
    let temp = TempDir::new("refresh-models");
    let runtime = load_runtime(&temp, REFRESH_EXTENSION_TS).await;

    let defs = runtime.provider_defs();
    let def = defs
        .iter()
        .find(|def| def.get("id").and_then(Value::as_str) == Some("dirprov"))
        .expect("provider definition recorded");
    assert_eq!(
        def.pointer("/config/refreshModels"),
        Some(&json!(true)),
        "the refreshModels marker rides the JSON definition"
    );

    // The session host triggers the same event+shape at registration
    // (`provider:refreshModels`); assert the hook result rides the outcome.
    let mut payload = json!({ "providerId": "dirprov" });
    let outcome = runtime
        .emit("provider:refreshModels", &mut payload)
        .await
        .expect("emit");
    assert_eq!(
        outcome.result.get("providerId").and_then(Value::as_str),
        Some("dirprov")
    );
    let models = outcome
        .result
        .get("models")
        .and_then(Value::as_array)
        .expect("models array in the outcome");
    let ids: Vec<&str> = models
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(ids, vec!["fresh-a", "fresh-b"]);
}

/// The FIRST prompt must sync provider definitions ONCE when no
/// `session_start` handler is declared: the sync at the top of the run already
/// applied every registration, and nothing can register between the two syncs
/// (the `input` event is dispatched after the lifecycle block). The former
/// duplicate sync re-ran every `refreshModels` refresh on the first prompt —
/// a duplicate network call / model-store write for real extensions.
#[tokio::test(flavor = "current_thread")]
async fn first_prompt_syncs_provider_definitions_once_without_session_start_handlers() {
    let temp = TempDir::new("provider-sync-once");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
          globalThis.__refreshes = 0;
          pi.registerProvider("onceprov", {
            baseUrl: "https://once.example.com/v1",
            api: "openai-chat",
            models: [{ id: "stale", name: "Stale", contextWindow: 1, maxTokens: 1, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
            refreshModels: async () => {
              globalThis.__refreshes = (globalThis.__refreshes ?? 0) + 1;
              return [{ id: "fresh", name: "Fresh", contextWindow: 1000, maxTokens: 100, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }];
            },
          });
        }
        "#,
    )
    .await;

    let mut registry_inner = ProviderRegistry::default();
    registry_inner.set_provider(Arc::new(StaticProvider {
        id: "staticprov",
        models: vec![fixture_model("static-a", "https://api.example.com/v1")],
    }));
    let registry = Arc::new(registry_inner);
    let registry_for_assert = Arc::clone(&registry);
    let mut session_model = fixture_model("static-a", "https://api.example.com/v1");
    session_model.provider = "staticprov".to_string();
    let session = session_with_extensions(
        Arc::clone(&registry),
        session_model,
        runtime.clone(),
        &temp.0,
    );

    let _ = session.prompt("hello", Arc::new(|_| {})).await;

    let refreshes = probe(&runtime, "globalThis.__refreshes ?? 0").await;
    assert_eq!(
        refreshes,
        json!(1),
        "the first prompt must apply provider definitions once; a duplicate sync re-runs refreshModels"
    );
    let models = registry_for_assert.get_provider_models("onceprov");
    assert_eq!(
        models.len(),
        1,
        "the single sync must register the provider"
    );
    assert_eq!(models[0].id, "fresh");
    session.shutdown().await;
}

/// A `refreshModels` hook is a network round trip, and provider sync runs on
/// every prompt: the hook must run at most once per window for an unchanged
/// definition (the first prompt runs it, the next prompt inside the window
/// reuses the refreshed/registered models without emitting again).
#[tokio::test(flavor = "current_thread")]
async fn refresh_models_hook_runs_at_most_once_per_window() {
    let temp = TempDir::new("refresh-models-window");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
          globalThis.__windowRefreshes = 0;
          pi.registerProvider("windowprov", {
            baseUrl: "https://window.example.com/v1",
            api: "openai-chat",
            models: [{ id: "window-stale", name: "Window stale", contextWindow: 1, maxTokens: 1, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
            refreshModels: async () => {
              globalThis.__windowRefreshes = (globalThis.__windowRefreshes ?? 0) + 1;
              return [{ id: "window-fresh", name: "Window fresh", contextWindow: 1000, maxTokens: 100, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }];
            },
          });
        }
        "#,
    )
    .await;

    let mut registry_inner = ProviderRegistry::default();
    registry_inner.set_provider(Arc::new(StaticProvider {
        id: "staticprov",
        models: vec![fixture_model("static-a", "https://api.example.com/v1")],
    }));
    let registry = Arc::new(registry_inner);
    let registry_for_assert = Arc::clone(&registry);
    let mut session_model = fixture_model("static-a", "https://api.example.com/v1");
    session_model.provider = "staticprov".to_string();
    let session = session_with_extensions(
        Arc::clone(&registry),
        session_model,
        runtime.clone(),
        &temp.0,
    );

    let _ = session.prompt("hello", Arc::new(|_| {})).await;
    assert_eq!(
        probe(&runtime, "globalThis.__windowRefreshes ?? 0").await,
        json!(1),
        "the first prompt runs the refresh"
    );
    assert_eq!(
        registry_for_assert.get_provider_models("windowprov")[0].id,
        "window-fresh",
        "the refreshed models are registered"
    );

    // Second prompt inside the window: no second network callback, and the
    // refreshed list is not replaced by the configured stale one.
    let _ = session.prompt("again", Arc::new(|_| {})).await;
    assert_eq!(
        probe(&runtime, "globalThis.__windowRefreshes ?? 0").await,
        json!(1),
        "a prompt inside the refresh window must not re-run refreshModels"
    );
    assert_eq!(
        registry_for_assert.get_provider_models("windowprov")[0].id,
        "window-fresh",
        "the previously refreshed models survive the gated sync"
    );
    session.shutdown().await;
}

/// With a `session_start` handler that registers a provider, the
/// post-`session_start` sync still runs: the provider it registered (and its
/// `refreshModels` result) is visible after the FIRST prompt, and the refresh
/// callback ran exactly once (the registration did not exist for the sync at
/// the top of the run).
#[tokio::test(flavor = "current_thread")]
async fn session_start_registration_is_synced_once_after_the_first_prompt() {
    let temp = TempDir::new("provider-sync-handler");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
          globalThis.__lateRefreshes = 0;
          pi.on("session_start", () => {
            pi.registerProvider("lateprov", {
              baseUrl: "https://late.example.com/v1",
              api: "openai-chat",
              models: [{ id: "late-stale", name: "Late stale", contextWindow: 1, maxTokens: 1, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
              refreshModels: async () => {
                globalThis.__lateRefreshes = (globalThis.__lateRefreshes ?? 0) + 1;
                return [{ id: "late-fresh", name: "Late fresh", contextWindow: 1000, maxTokens: 100, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }];
              },
            });
          });
        }
        "#,
    )
    .await;

    let mut registry_inner = ProviderRegistry::default();
    registry_inner.set_provider(Arc::new(StaticProvider {
        id: "staticprov",
        models: vec![fixture_model("static-a", "https://api.example.com/v1")],
    }));
    let registry = Arc::new(registry_inner);
    let registry_for_assert = Arc::clone(&registry);
    let mut session_model = fixture_model("static-a", "https://api.example.com/v1");
    session_model.provider = "staticprov".to_string();
    let session = session_with_extensions(
        Arc::clone(&registry),
        session_model,
        runtime.clone(),
        &temp.0,
    );

    let _ = session.prompt("hello", Arc::new(|_| {})).await;

    let refreshes = probe(&runtime, "globalThis.__lateRefreshes ?? 0").await;
    assert_eq!(
        refreshes,
        json!(1),
        "the session_start-time registration must be synced exactly once"
    );
    let models = registry_for_assert.get_provider_models("lateprov");
    assert_eq!(
        models.len(),
        1,
        "the post-session_start sync must publish the late provider"
    );
    assert_eq!(models[0].id, "late-fresh");
    session.shutdown().await;
}

// ---------------------------------------------------------------------------
// 3. baseUrl-only override (#12)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn base_url_only_registration_overrides_existing_provider_url() {
    let temp = TempDir::new("base-url-override");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
          pi.registerProvider("staticprov", { baseUrl: "https://proxy.example.com/v1" });
        }
        "#,
    )
    .await;

    let mut registry_inner = ProviderRegistry::default();
    registry_inner.set_provider(Arc::new(StaticProvider {
        id: "staticprov",
        models: vec![
            fixture_model("static-a", "https://api.example.com/v1"),
            fixture_model("static-b", "https://api.example.com/v1"),
        ],
    }));
    let registry = Arc::new(registry_inner);
    let registry_for_assert = Arc::clone(&registry);

    let mut session_model = fixture_model("static-a", "https://api.example.com/v1");
    session_model.provider = "staticprov".to_string();
    let session = session_with_extensions(Arc::clone(&registry), session_model, runtime, &temp.0);
    let _ = session.prompt("hello", Arc::new(|_| {})).await;

    // The dynamic overlay replaces the URL for the existing models and wins
    // over the static registry entry on lookup (upstream applyExtension).
    let models = registry_for_assert.get_provider_models("staticprov");
    assert_eq!(models.len(), 2, "both existing models survive");
    for model in &models {
        assert_eq!(
            model.base_url, "https://proxy.example.com/v1",
            "model {} url overridden",
            model.id
        );
    }
    assert!(
        matches!(
            model_api(&models).as_str(),
            "openai-chat" | "openai-completions"
        ),
        "model api untouched by the url override"
    );
}

fn model_api(models: &[Model]) -> String {
    models
        .first()
        .map(|model| model.api.clone())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 4. google-generative-ai dynamic routing (#22)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn google_generative_ai_dynamic_provider_routes_to_the_google_adapter() {
    let registry = Arc::new(ProviderRegistry::default());
    let provider = registry
        .register_dynamic_provider_shared(
            "my-gemini",
            DynamicProviderConfig {
                base_url: "https://generativelanguage.example.com".to_string(),
                api: "google-generative-ai".to_string(),
                // The model carries NO api of its own so the provider-level
                // `api` applies (upstream: `definition.api ?? providerConfig.api`
                // — a per-model api would win and route to OpenAI instead).
                models: vec![Model {
                    api: String::new(),
                    ..fixture_model("gemini-x", "https://generativelanguage.example.com")
                }],
                ..DynamicProviderConfig::default()
            },
        )
        .expect("dynamic registration");
    // build_provider_from_models dispatches on the model api id: the Google
    // adapter resolves GEMINI/GOOGLE env keys (the OpenAI-compatible fallback
    // would carry OPENAI_API_KEY instead).
    let auth = provider.api_key_auth();
    assert!(
        auth.iter()
            .any(|entry| entry.env_vars.iter().any(|name| name == "GEMINI_API_KEY")),
        "google adapter auth surface missing: {:?}",
        auth.iter().map(|entry| &entry.env_vars).collect::<Vec<_>>()
    );
    assert!(
        !auth
            .iter()
            .any(|entry| entry.env_vars.iter().any(|name| name == "OPENAI_API_KEY")),
        "dynamic google provider must not fall back to the OpenAI adapter"
    );
}

// ---------------------------------------------------------------------------
// 5. ctx.waitForIdle (#13)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn wait_for_idle_resolves_when_idle_and_blocks_until_run_end() {
    let temp = TempDir::new("wait-for-idle");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
          pi.registerCommand("wait-idle", {
            handler: async (_args, ctx) => {
              await ctx.waitForIdle();
              globalThis.__idleDone = (globalThis.__idleDone ?? 0) + 1;
              globalThis.__idleResolvedAt = Date.now();
            },
          });
        }
        "#,
    )
    .await;

    // Idle: resolves immediately.
    runtime
        .dispatch_command("wait-idle", "")
        .await
        .expect("idle wait resolves");
    let done = probe(&runtime, "globalThis.__idleDone ?? 0").await;
    assert_eq!(done, json!(1));

    // Streaming: the wait blocks until the session-pushed flag clears. The
    // resolved-at stamp (not a mid-flight probe) proves the ordering: a
    // host-fn future suspended inside the QuickJS job executor must not be
    // probed with an interleaved eval (the executor loses the parked
    // future's wakeup), so the wait is observed only after it settles.
    runtime.set_session_streaming(true);
    let dispatch = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.dispatch_command("wait-idle", "").await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;

    let cleared_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    runtime.set_session_streaming(false);
    let outcome = tokio::time::timeout(Duration::from_secs(5), dispatch)
        .await
        .expect("dispatch finishes after idle")
        .expect("dispatch join");
    assert!(outcome.is_ok(), "dispatch ok: {:?}", outcome);
    let done = probe(&runtime, "globalThis.__idleDone ?? 0").await;
    assert_eq!(done, json!(2), "waitForIdle resolved once the run ended");
    let resolved_at_ms = probe(&runtime, "globalThis.__idleResolvedAt")
        .await
        .as_u64()
        .expect("resolved-at stamp");
    assert!(
        resolved_at_ms + 50 >= cleared_at_ms,
        "waitForIdle must not resolve while streaming: resolved at {resolved_at_ms}, \
         cleared at {cleared_at_ms}"
    );
}

// ---------------------------------------------------------------------------
// 6. autocomplete factory chain (#30)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn autocomplete_factories_chain_last_registration_outermost() {
    let temp = TempDir::new("autocomplete-chain");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
          // addAutocompleteProvider rides ctx.ui (upstream ExtensionUIContext,
          // types.ts:243), not the pi API surface.
          pi.registerCommand("chain-autocomplete", {
            handler: async (_args, ctx) => {
              ctx.ui.addAutocompleteProvider((current) => ({
                getSuggestions: async (context) => {
                  const previous = current ? await current.getSuggestions(context) : [];
                  return (previous ?? []).concat([{ value: "inner" }]);
                },
              }));
              ctx.ui.addAutocompleteProvider((current) => ({
                getSuggestions: async (context) => {
                  const previous = current ? await current.getSuggestions(context) : [];
                  return (previous ?? []).concat([{ value: "outer:" + context.prefix }]);
                },
              }));
            },
          });
        }
        "#,
    )
    .await;

    runtime
        .dispatch_command("chain-autocomplete", "")
        .await
        .expect("chain registration dispatches");

    let items = runtime
        .extension_suggestions(r#"{"prefix":"x"}"#)
        .await
        .expect("suggestions");
    let values: Vec<String> = items
        .iter()
        .filter_map(|item| item.get("value").and_then(Value::as_str).map(String::from))
        .collect();
    // The first-registered factory wrapped the null shim (its `current`
    // contributes nothing); the second wrapped the first: inner then outer.
    assert_eq!(values, vec!["inner", "outer:x"]);
}

// ---------------------------------------------------------------------------
// 7. mid-session definition gate (host registry refresh)
// ---------------------------------------------------------------------------

/// Load one MORE extension file into an already-running runtime: the
/// registration lands with no host event and no `/reload`, so only the next
/// `refresh_registry_if_changed` can publish it. Mirrors the late fixture load
/// in `tests/mcp_integration.rs`
/// (`mcp_server_change_is_delivered_by_the_registration_rebuild`: write the
/// late file, `js.load(...)`, then assert the shared registry only learns it
/// at a rebuild).
async fn load_late(runtime: &ExtensionHandle, dir: &TempDir, name: &str, source: &str) {
    let path = dir.0.join(name);
    std::fs::write(&path, source).expect("write late fixture");
    let result = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load late fixture");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
}

/// Emit an event NO plugin observes — `tests/mcp_integration.rs`'s probe-event
/// pattern (`!host.has_handlers("agent_settled")`): the only host step it can
/// run is the emit-tail `refresh_registry_if_changed`, so anything published
/// after this call came from that gate, never from the dispatch.
async fn emit_without_consumers(host: &PluginHost) {
    assert!(
        !host.has_handlers("agent_settled"),
        "the probe event must have no consumer"
    );
    let mut payload = json!({ "type": "agent_settled" });
    let outcome = host
        .emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert!(
        outcome.errors.is_empty(),
        "emit errors: {:?}",
        outcome.errors
    );
}

/// `pi.registerProvider` from a `session_start` handler — the literal shape of
/// the regression: the registration lands AFTER `PluginHost::build`
/// snapshotted the JS defs, it moves only the provider class (the extension
/// registers no JS tool at all), and the consumer that turns defs into real
/// registry providers (`sync_plugin_providers`, which reads only
/// `plugins.registry()`) runs right after that dispatch.
///
/// Revert caught (narrow, tool-names-only gate): the emit tail sees no
/// tool-name/MCP-server change and skips the rebuild, the post-`session_start`
/// sync keeps reading the pre-registration snapshot, and
/// `get_provider_models("lateprov")` stays empty.
#[tokio::test(flavor = "current_thread")]
async fn session_start_provider_registration_reaches_the_provider_registry() {
    let temp = TempDir::new("session-start-provider");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
          pi.on("session_start", () => {
            pi.registerProvider("lateprov", {
              baseUrl: "https://late.example.com/v1",
              api: "openai-chat",
              models: [{
                id: "late-model",
                name: "Late model",
                contextWindow: 1000,
                maxTokens: 100,
                cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
              }],
            });
          });
        }
        "#,
    )
    .await;

    // The run itself needs a resolvable provider/model; its stream ends
    // immediately (the StaticProvider pattern of test 3 above), so the prompt
    // completes without touching the network.
    let mut registry_inner = ProviderRegistry::default();
    registry_inner.set_provider(Arc::new(StaticProvider {
        id: "staticprov",
        models: vec![fixture_model("static-a", "https://api.example.com/v1")],
    }));
    let registry = Arc::new(registry_inner);
    let registry_for_assert = Arc::clone(&registry);

    let mut session_model = fixture_model("static-a", "https://api.example.com/v1");
    session_model.provider = "staticprov".to_string();
    let session = session_with_extensions(Arc::clone(&registry), session_model, runtime, &temp.0);
    assert!(
        registry_for_assert
            .get_provider_models("lateprov")
            .is_empty(),
        "precondition: `lateprov` exists only as an un-published extension def"
    );

    // The first prompt dispatches `session_start` (the emit tail is the
    // rebuild point) and then re-syncs providers from the shared registry.
    let _ = session.prompt("hello", Arc::new(|_| {})).await;

    let models = registry_for_assert.get_provider_models("lateprov");
    assert_eq!(
        models.len(),
        1,
        "the session_start-registered provider must reach the provider registry through \
         `sync_plugin_providers`; a gate that fingerprints only the JS tool names skips the \
         rebuild, so the def never becomes a real provider"
    );
    assert_eq!(models[0].id, "late-model");
    assert_eq!(models[0].base_url, "https://late.example.com/v1");
    session.shutdown().await;
}

/// The registry-publication seam of the same revert, without the session path:
/// a provider-only extension loaded after the first build is invisible until a
/// rebuild, and the one no-consumer emit whose only host step is the emit-tail
/// refresh must publish it into `PluginRegistry::provider_defs()` — the list
/// every provider/virtual-model consumer reads.
///
/// Revert caught (narrow, tool-names-only gate): no JS tool name moves, the
/// rebuild is skipped, and the provider def never joins the shared registry.
#[tokio::test(flavor = "current_thread")]
async fn late_provider_only_registration_is_published_by_the_refresh_gate() {
    let temp = TempDir::new("late-provider");
    let runtime = load_runtime(&temp, "export default function (pi) { void pi; }").await;
    let host = PluginHost::build(Some(runtime.clone()), Vec::new());
    assert!(
        host.registry().provider_defs().is_empty(),
        "precondition: no provider defs before the late load"
    );

    load_late(
        &runtime,
        &temp,
        "late-provider.ts",
        r#"
        export default function (pi) {
          pi.registerProvider("lateprov", { baseUrl: "https://late.example.com/v1" });
        }
        "#,
    )
    .await;
    assert!(
        host.registry().provider_defs().is_empty(),
        "the def lives in the runtime only; the shared registry keeps serving its old snapshot"
    );

    emit_without_consumers(&host).await;

    let defs = host.registry().provider_defs();
    let def = defs
        .iter()
        .find(|def| def.get("id").and_then(Value::as_str) == Some("lateprov"))
        .unwrap_or_else(|| {
            panic!("provider-only registration missed by the refresh gate: {defs:?}")
        });
    assert_eq!(
        def.pointer("/config/baseUrl"),
        Some(&json!("https://late.example.com/v1")),
        "the config must ride the `{{id, config}}` definition into the registry"
    );
    host.shutdown().await;
}

/// The four NON-provider definition classes (commands, flags, shortcuts,
/// virtual models) registered mid-session (late-loaded file, no event, no
/// reload) must reach the shared registry through the same emit-tail rebuild.
/// A single class move per gate version is enough to pin the class coverage;
/// all four ride one fixture so a fingerprint that omits ANY of them names
/// itself in the failure message.
///
/// Revert caught (narrow, tool-names-only gate): `pi.registerCommand` /
/// `registerFlag` / `registerShortcut` / `registerVirtualModel` all leave the
/// tool-name set untouched, so no rebuild runs and every class stays invisible
/// to its consumer (command resolution, flag enumeration, keybinding
/// resolution, the virtual-model overlay).
#[tokio::test(flavor = "current_thread")]
async fn non_provider_mid_session_registrations_trigger_the_registry_rebuild() {
    let temp = TempDir::new("late-def-classes");
    let runtime = load_runtime(&temp, "export default function (pi) { void pi; }").await;
    let host = PluginHost::build(Some(runtime.clone()), Vec::new());

    load_late(
        &runtime,
        &temp,
        "late-defs.ts",
        r#"
        export default function (pi) {
          pi.registerCommand("late-cmd", { description: "late command", handler: async () => {} });
          pi.registerFlag("late-flag", { type: "string", description: "late flag", default: "late-value" });
          pi.registerShortcut("ctrl+alt+shift+l", { handler: async () => {} });
          pi.registerVirtualModel({
            id: "late-virtual",
            name: "Late virtual",
            physical: { provider: "staticprov", id: "static-a" },
          });
        }
        "#,
    )
    .await;
    assert_eq!(
        non_provider_class_visibility(&host.registry()),
        vec![
            ("command", false),
            ("flag", false),
            ("shortcut", false),
            ("virtual model", false)
        ],
        "precondition: nothing is published before a rebuild"
    );

    emit_without_consumers(&host).await;

    let after = non_provider_class_visibility(&host.registry());
    let missing: Vec<&str> = after
        .iter()
        .filter(|(_, visible)| !visible)
        .map(|(class, _)| *class)
        .collect();
    assert!(
        missing.is_empty(),
        "mid-session registrations missed by the rebuild gate: {missing:?} — the gate must \
         fingerprint every class `JsExtensionsPlugin::register` imports, not only the JS tool \
         names (a tool-names-only gate skips this rebuild and consumers read the stale registry)"
    );
    host.shutdown().await;
}

/// Is each class the late fixture registers visible in the shared registry?
/// Read through the public consumer surfaces: `resolved_commands` (command
/// resolution), `RegisteredPlugin::registrations.flags` (flag enumeration),
/// `resolve_shortcuts` (keybinding resolution) and `virtual_model_defs` (the
/// virtual-model overlay source).
fn non_provider_class_visibility(registry: &PluginRegistry) -> Vec<(&'static str, bool)> {
    let command = registry
        .resolved_commands()
        .iter()
        .any(|command| command.def.name == "late-cmd");
    let flag = registry.plugins().iter().any(|plugin| {
        plugin
            .registrations
            .flags
            .iter()
            .any(|flag| flag.name == "late-flag")
    });
    let shortcut = registry
        .resolve_shortcuts(&Default::default())
        .0
        .iter()
        .any(|shortcut| shortcut.def.key == "ctrl+alt+shift+l");
    let virtual_model = registry
        .virtual_model_defs()
        .iter()
        .any(|def| def.get("id").and_then(Value::as_str) == Some("late-virtual"));
    vec![
        ("command", command),
        ("flag", flag),
        ("shortcut", shortcut),
        ("virtual model", virtual_model),
    ]
}

/// The performance property the definition fingerprint exists for: a refresh
/// with NO JS definition change must not rebuild. `PluginHost::registry()`
/// hands out the live `Arc` and a rebuild stores a NEW `Arc`, so `Arc::ptr_eq`
/// across two consecutive no-change refreshes pins "no rebuild happened"
/// exactly (the compare gate returns without touching the registry).
///
/// Revert caught (gate dropped or made unstable): rebuilding on every refresh,
/// or a fingerprint that can never compare equal (unordered collection /
/// non-deterministic serialization), replaces the `Arc` between the two
/// refreshes. The positive control below proves the observable is not vacuous:
/// a real definition-class change still swaps the `Arc`. (The narrow
/// tool-names-only revert is the opposite direction and is pinned by the three
/// tests above.)
#[tokio::test(flavor = "current_thread")]
async fn a_refresh_without_js_def_changes_does_not_rebuild_the_registry() {
    let temp = TempDir::new("no-change-refresh");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
          pi.registerProvider("settled-prov", { baseUrl: "https://settled.example.com/v1" });
        }
        "#,
    )
    .await;
    let host = PluginHost::build(Some(runtime.clone()), Vec::new());

    // First refresh: settle any one-time runtime state (the fingerprint is
    // already correct at build time, so this pass must not rebuild either).
    emit_without_consumers(&host).await;
    let settled = host.registry();

    // Second consecutive refresh, no JS def change: the SAME registry `Arc`.
    emit_without_consumers(&host).await;
    let unchanged = host.registry();
    assert!(
        Arc::ptr_eq(&settled, &unchanged),
        "a no-change refresh rebuilt the registry: the fingerprint compare must skip the \
         rebuild, and a gate that rebuilds unconditionally (or whose fingerprints never \
         compare equal) replaces the Arc here"
    );

    // Positive control: a definition class BOTH gate versions watch (MCP
    // server names) still forces a rebuild, so the `ptr_eq` check above is a
    // real observable and not a vacuous pass.
    load_late(
        &runtime,
        &temp,
        "late-ctrl-server.ts",
        r#"
        export default function (pi) {
          pi.registerMcpServer("late-ctrl-server", {
            command: "definitely-not-a-real-command-24680",
            enabled: false,
          });
        }
        "#,
    )
    .await;
    emit_without_consumers(&host).await;
    let rebuilt = host.registry();
    assert!(
        !Arc::ptr_eq(&unchanged, &rebuilt),
        "positive control: a real definition change must replace the registry Arc"
    );
    assert!(
        rebuilt.plugins().iter().any(|plugin| plugin
            .registrations
            .mcp_servers
            .iter()
            .any(|def| def.get("name").and_then(Value::as_str) == Some("late-ctrl-server"))),
        "positive control: the rebuilt registry carries the late MCP server def"
    );
    host.shutdown().await;
}
