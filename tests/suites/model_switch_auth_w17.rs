//! W17-C: switching models must send the NEW provider's credential.
//!
//! The reported bug: `/model` to a model on another provider kept sending the
//! key the session was constructed with, so every request 401'd even though
//! each provider's key was correct. The session now resolves the credential for
//! the provider of the model a request actually runs under
//! (`AgentSession::request_api_key_for`), and the captured
//! `AgentSessionConfig::api_key` is scoped to the provider it was resolved for.
//!
//! Everything here is hermetic: scripted providers capture the
//! `StreamOptions` they are handed (never a network call), and `models.json`
//! is written into a scratch agent directory pinned with `PIR_AGENT_DIR`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;

use crate::common::AgentDirEnvGuard;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn model(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "w17-fake".to_string(),
        provider: provider.to_string(),
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

/// Every `api_key` a scripted provider was handed, in order (one entry per
/// request; `None` records a request that carried no credential at all).
type SeenKeys = Arc<Mutex<Vec<Option<String>>>>;

/// A scripted provider that answers immediately and records the credential of
/// each request.
///
/// `api_key_auth` is empty (a keyless scripted provider), so the registry's own
/// resolver contributes nothing: the only credential a request can carry is the
/// one the SESSION resolved, which is exactly what these tests observe. The
/// session dispatch path (`registry_event_stream` → `stream_unchecked`) does not
/// consult the registry resolver at all, so this keeps the fixtures hermetic.
struct RecordingProvider {
    provider: String,
    models: Vec<Model>,
    seen: SeenKeys,
}

impl Provider for RecordingProvider {
    fn id(&self) -> &str {
        &self.provider
    }
    fn name(&self) -> &str {
        &self.provider
    }
    fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
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

/// Two providers (`w17a`, `w17b`), each with two models, plus the captured
/// credentials of every request.
fn two_provider_registry() -> (Arc<ProviderRegistry>, SeenKeys, SeenKeys) {
    let seen_a: SeenKeys = Arc::new(Mutex::new(Vec::new()));
    let seen_b: SeenKeys = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(RecordingProvider {
        provider: "w17a".to_string(),
        models: vec![model("w17a", "a-one"), model("w17a", "a-two")],
        seen: Arc::clone(&seen_a),
    }));
    registry.set_provider(Arc::new(RecordingProvider {
        provider: "w17b".to_string(),
        models: vec![model("w17b", "b-one")],
        seen: Arc::clone(&seen_b),
    }));
    (Arc::new(registry), seen_a, seen_b)
}

fn session_config(
    registry: Arc<ProviderRegistry>,
    model: Model,
    key: Option<&str>,
) -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: registry,
        model,
        api_key: key.map(str::to_string),
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: Some("w17-c model-switch credential".to_string()),
        cwd: None,
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

/// Write `<dir>/models.json` with one `apiKey` per `(provider, key)` pair.
fn write_models_json(dir: &std::path::Path, keys: &[(&str, &str)]) {
    let providers: BTreeMap<String, serde_json::Value> = keys
        .iter()
        .map(|(provider, key)| {
            (
                (*provider).to_string(),
                serde_json::json!({ "apiKey": key }),
            )
        })
        .collect();
    std::fs::create_dir_all(dir).expect("create agent dir");
    std::fs::write(
        dir.join("models.json"),
        serde_json::json!({ "providers": providers }).to_string(),
    )
    .expect("write models.json");
}

/// Run one prompt to completion (its terminal message is discarded).
async fn prompt_once(session: &AgentSession) -> AssistantMessage {
    session
        .prompt(
            "hello",
            Arc::new(|_event: pir::agent::agent_loop::AgentEvent| {}),
        )
        .await
        .expect("prompt completes")
}

fn only_key(seen: &SeenKeys, what: &str) -> Option<String> {
    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        seen.len(),
        1,
        "{what}: expected exactly one request, got {seen:?}"
    );
    seen[0].clone()
}

// ---------------------------------------------------------------------------
// (a) a cross-provider switch sends THAT provider's credential
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cross_provider_switch_sends_the_new_providers_credential() {
    let guard = AgentDirEnvGuard::scratch("w17c-cross", None).await;
    let dir = guard.dir().expect("scratch agent dir").to_path_buf();
    write_models_json(
        &dir,
        &[("w17a", "key-from-models-a"), ("w17b", "key-from-models-b")],
    );

    let (registry, seen_a, seen_b) = two_provider_registry();
    let session = AgentSession::new(session_config(
        registry,
        model("w17a", "a-one"),
        // The session is constructed with provider A's key. Before the fix this
        // single value was put on EVERY request, so the switch below sent
        // "key-a" to provider B — a 401 whenever B's key was valid.
        Some("key-a"),
    ));

    prompt_once(&session).await;
    assert_eq!(only_key(&seen_a, "first request").as_deref(), Some("key-a"));

    let target = session.find_model("w17b", "b-one").expect("model b-one");
    session.set_model(target).await.expect("set_model");

    prompt_once(&session).await;
    assert_eq!(
        only_key(&seen_b, "post-switch request").as_deref(),
        Some("key-from-models-b"),
        "a model switch must send the NEW provider's credential, never the previous provider's"
    );
}

// ---------------------------------------------------------------------------
// (b) an explicit api_key override still wins for its own provider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explicit_api_key_wins_for_its_own_provider() {
    let guard = AgentDirEnvGuard::scratch("w17c-explicit", None).await;
    let dir = guard.dir().expect("scratch agent dir").to_path_buf();
    // models.json also configures provider A: the explicit key must beat it.
    write_models_json(&dir, &[("w17a", "key-from-models-a")]);

    let (registry, seen_a, _seen_b) = two_provider_registry();
    let session = AgentSession::new(session_config(
        registry,
        model("w17a", "a-one"),
        Some("explicit-a"),
    ));

    prompt_once(&session).await;
    assert_eq!(
        only_key(&seen_a, "explicit override").as_deref(),
        Some("explicit-a"),
        "an explicitly supplied api_key must keep winning for its provider"
    );
}

// ---------------------------------------------------------------------------
// (c) a same-provider model switch is unchanged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_provider_switch_keeps_the_same_credential() {
    let guard = AgentDirEnvGuard::scratch("w17c-same", None).await;
    let dir = guard.dir().expect("scratch agent dir").to_path_buf();
    write_models_json(&dir, &[("w17a", "key-from-models-a")]);

    let (registry, seen_a, _seen_b) = two_provider_registry();
    let session = AgentSession::new(session_config(
        registry,
        model("w17a", "a-one"),
        Some("explicit-a"),
    ));

    prompt_once(&session).await;
    let target = session.find_model("w17a", "a-two").expect("model a-two");
    session.set_model(target).await.expect("set_model");
    prompt_once(&session).await;

    let seen = seen_a
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        seen,
        vec![
            Some("explicit-a".to_string()),
            Some("explicit-a".to_string())
        ],
        "a same-provider switch must keep working with no extra work"
    );
}

// ---------------------------------------------------------------------------
// (d) a provider with no credential yields no credential, not another's
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_without_a_credential_sends_none() {
    let guard = AgentDirEnvGuard::scratch("w17c-none", None).await;
    let dir = guard.dir().expect("scratch agent dir").to_path_buf();
    // Only provider A is configured: provider B has no key anywhere.
    write_models_json(&dir, &[("w17a", "key-from-models-a")]);

    let (registry, _seen_a, seen_b) = two_provider_registry();
    let session = AgentSession::new(session_config(
        registry,
        model("w17a", "a-one"),
        Some("key-a"),
    ));

    let target = session.find_model("w17b", "b-one").expect("model b-one");
    session.set_model(target).await.expect("set_model");
    prompt_once(&session).await;

    assert_eq!(
        only_key(&seen_b, "unconfigured provider").as_deref(),
        None,
        "an unconfigured provider must get NO key — sending another provider's key is the 401 bug"
    );
}

/// The registry's own auth surface reports the preserved outcome for a provider
/// with no credential: "not configured" (`Ok(None)`), which the dispatch path
/// turns into `Provider is not configured` — never an unauthenticated request.
#[tokio::test]
async fn unconfigured_provider_is_reported_as_not_configured() {
    let (registry, _seen_a, _seen_b) = two_provider_registry();
    let model = model("w17b", "b-one");
    let auth = registry.get_auth(&model).await.expect("auth resolution");
    assert!(
        auth.is_none(),
        "a provider whose only credential source is unset resolves to not-configured"
    );
}

/// `models.json` keys are resolved for the SESSION'S agent directory and are
/// only read again when that file actually changes (stamp-cached, bounded), so
/// a per-request resolution stays cheap.
#[tokio::test]
async fn models_json_keys_are_cached_per_agent_directory() {
    let guard = AgentDirEnvGuard::scratch("w17c-cache", None).await;
    let dir = guard.dir().expect("scratch agent dir").to_path_buf();
    write_models_json(&dir, &[("w17a", "first-key")]);

    let (registry, seen_a, _seen_b) = two_provider_registry();
    let session = AgentSession::new(session_config(registry, model("w17a", "a-one"), None));

    prompt_once(&session).await;
    // Rewriting the file changes its stamp, so the next request re-reads it:
    // a credential edited outside the process is picked up without a restart.
    write_models_json(&dir, &[("w17a", "second-key")]);
    prompt_once(&session).await;

    let seen = seen_a
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        seen,
        vec![
            Some("first-key".to_string()),
            Some("second-key".to_string())
        ],
        "the stamp cache must serve a hit and re-read after an edit: {seen:?}"
    );
}
