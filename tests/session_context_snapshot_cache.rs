//! `ctx.sessionManager` snapshot push cache (`agent_session/session_ctx.rs`).
//!
//! The snapshot push runs before every prompt and again after it settles. The
//! second push used to rebuild the whole session-derived payload — context
//! entries (a full projection materialization + serialization), the branch, the
//! full entry tree, the per-entry projection copy, the label map, and the
//! session-directory listing — even when nothing had changed. The push now
//! fingerprints the live state (cheap borrowed session reads + the scalar
//! snapshot cells) and skips the rebuild when the runtime already holds the
//! identical snapshot.
//!
//! The tests pin:
//! - two consecutive pushes with no session mutation leave the runtime snapshot
//!   byte-identical and do not push it a second time to plugins, and the second
//!   push reads NO session file for the listing (`session_list_file_reads`);
//! - appending a session entry invalidates the fingerprint: the next push
//!   rebuilds and the new entry is visible in the snapshot;
//! - the model catalog / authenticated-provider half is cached by registry
//!   identity + credential-file stamps (R1-L1-008): a cache hit pushes zero
//!   model rows through `update_model_registry` and performs zero
//!   `load_pi_environment` loads, while a `models.json` edit, a registry
//!   registration or a changed plugin-provider definition invalidates it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::{
    ApiKeyAuth, AuthEvent, AuthInteraction, AuthPrompt, FileAuthStorage, LoginOptions,
};
use pir::ai::error::AiResult;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::ai::virtual_::{PhysicalModelRef, VirtualModelRegistration};
use pir::extensions::{ExtensionConfig, ExtensionHandle, SessionContextSnapshot};
use pir::plugins::{AgentPlugin, PluginInfo, RegistrationContext};
use pir::session_manager::{
    reset_session_list_file_reads, session_list_file_reads, SessionManager,
};

/// Serializes every test in this file: the credential-cache test pins the
/// process-global `PIR_AGENT_DIR` while it runs, and the other tests' pushes
/// read the same environment for their auth fingerprint.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Pins `PIR_AGENT_DIR` for one test and restores it (panic included) on
/// drop, while holding [`ENV_LOCK`] for the test's whole lifetime.
struct AgentDirOverride {
    previous: Option<std::ffi::OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl AgentDirOverride {
    fn new(agent_dir: &Path) -> Self {
        let lock = env_lock();
        let previous = std::env::var_os("PIR_AGENT_DIR");
        std::env::set_var("PIR_AGENT_DIR", agent_dir);
        Self {
            previous,
            _lock: lock,
        }
    }
}

impl Drop for AgentDirOverride {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
            None => std::env::remove_var("PIR_AGENT_DIR"),
        }
    }
}

/// Sets one credential-template environment variable and restores it on
/// drop. Callers hold [`ENV_LOCK`] through an [`AgentDirOverride`], so this
/// guard does not lock (avoiding a self-deadlock).
struct EnvVarOverride {
    name: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarOverride {
    fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for EnvVarOverride {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-ctx-snapshot-cache-{label}-{}-{}",
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

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 2,
        ..Default::default()
    })
}

/// An assistant message carrying a provider usage record: the snapshot's
/// `context_tokens` contract is the LAST usage record, not a lifetime sum.
fn assistant_message_with_usage(text: &str, total_tokens: u64) -> Message {
    let mut message = assistant_message(text);
    if let Message::Assistant(assistant) = &mut message {
        assistant.usage = Usage {
            total_tokens,
            ..Default::default()
        };
    }
    message
}

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
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

/// A model with a stable id/provider, used to make the serialized catalog
/// size observable (`fake_model` alone is a single entry).
fn catalog_model(provider: &str, index: usize) -> Model {
    let mut model = fake_model();
    model.id = format!("{provider}-{index}");
    model.name = format!("{provider} model {index}");
    model.provider = provider.to_owned();
    model
}

/// Provider exposing an immutable model list (no refresh), the shape a
/// catalog/models.json-composed provider has in the registry.
struct CatalogProvider {
    id: String,
    name: String,
    models: Vec<Model>,
}

impl CatalogProvider {
    fn new(id: &str, model_count: usize) -> Self {
        Self {
            id: id.to_owned(),
            name: format!("{id} provider"),
            models: (0..model_count)
                .map(|index| catalog_model(id, index))
                .collect(),
        }
    }
}

impl Provider for CatalogProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        AssistantMessageEventStream::new()
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
}

/// Inline plugin registering one `pi.registerProvider` definition. The
/// session host re-applies it on every prompt through
/// `sync_plugin_providers`; an unchanged definition must short-circuit and
/// reuse the existing dynamic provider `Arc`.
struct GammaProviderPlugin {
    info: PluginInfo,
}

impl GammaProviderPlugin {
    fn new() -> Self {
        Self {
            info: PluginInfo::inline("gamma-provider"),
        }
    }
}

impl AgentPlugin for GammaProviderPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_provider(
            "gamma",
            serde_json::json!({
                "baseUrl": "http://localhost",
                "api": "openai-chat",
                "apiKey": "key-gamma",
                "models": [{
                    "id": "gamma-0",
                    "name": "Gamma model 0",
                    "contextWindow": 4096,
                    "maxTokens": 256,
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
                }]
            }),
        );
        Ok(())
    }
}

/// Inline plugin whose `pi.registerProvider` definition flips `streamSimple`
/// between registry rebuilds, emulating a plugin hot reload. The registry
/// rebuild re-runs `register()`, so the shared flag reaches the new definition
/// without touching the session.
struct MutableStreamProviderPlugin {
    info: PluginInfo,
    stream_simple: Arc<Mutex<bool>>,
}

impl MutableStreamProviderPlugin {
    fn new() -> Self {
        Self {
            info: PluginInfo::inline("mutable-stream-provider"),
            stream_simple: Arc::new(Mutex::new(false)),
        }
    }

    fn set_stream_simple(&self, enabled: bool) {
        *self
            .stream_simple
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = enabled;
    }
}

impl AgentPlugin for MutableStreamProviderPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        let mut config = serde_json::json!({
            "baseUrl": "http://localhost",
            "api": "openai-chat",
            "apiKey": "key-delta",
            "models": [{
                "id": "delta-0",
                "name": "Delta model 0",
                "contextWindow": 4096,
                "maxTokens": 256,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
            }]
        });
        if *self
            .stream_simple
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            config["streamSimple"] = serde_json::json!(true);
        }
        ctx.register_provider("delta", config);
        Ok(())
    }
}

/// Inline plugin whose `pi.registerProvider` definition can be withdrawn
/// without rebuilding the session. Simulates a child whose definitions are
/// removed between two provider syncs (SNAP6 P2-2): the old dynamic entry is
/// no longer reinstalled, but the session's cleanup ownership record must
/// still find it at teardown.
struct ToggleProviderPlugin {
    info: PluginInfo,
    enabled: Arc<Mutex<bool>>,
}

impl ToggleProviderPlugin {
    fn new() -> (Self, Arc<Mutex<bool>>) {
        let enabled = Arc::new(Mutex::new(true));
        (
            Self {
                info: PluginInfo::inline("toggle-provider"),
                enabled: Arc::clone(&enabled),
            },
            enabled,
        )
    }
}

impl AgentPlugin for ToggleProviderPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        if !*self
            .enabled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Ok(());
        }
        ctx.register_provider(
            "toggle",
            serde_json::json!({
                "baseUrl": "http://localhost",
                "api": "openai-chat",
                "apiKey": "key-toggle",
                "models": [{
                    "id": "toggle-0",
                    "name": "Toggle model 0",
                    "contextWindow": 4096,
                    "maxTokens": 256,
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
                }]
            }),
        );
        Ok(())
    }
}

fn try_registered_provider(registry: &ProviderRegistry, id: &str) -> Option<Arc<dyn Provider>> {
    registry
        .providers()
        .into_iter()
        .find(|provider| provider.id() == id)
}

fn registered_provider(registry: &ProviderRegistry, id: &str) -> Arc<dyn Provider> {
    try_registered_provider(registry, id).unwrap_or_else(|| panic!("provider `{id}` is registered"))
}

/// `streamSimple` installs a `JsStreamProvider`, whose api-key auth is the
/// always-satisfied extension-stream federation (upstream keeps the
/// credentials inside the JS callback). A plain dynamic provider exposes the
/// usual environment-backed auth entries instead.
fn is_js_stream_provider(provider: &dyn Provider) -> bool {
    provider
        .api_key_auth()
        .iter()
        .any(|auth| auth.name == "Extension streamSimple provider")
}

/// A JS extension registering `oauprov` with a plugin API key and an OAuth
/// flow whose login result names the runtime that executed it. Two runtimes
/// loading the same provider definition differ only in `label`, which makes a
/// cross-session OAuth binding observable through `flow.login()`'s credential.
fn oauth_extension_source(label: &str) -> String {
    format!(
        r#"
export default function (pi) {{
  pi.registerProvider("oauprov", {{
    baseUrl: "http://localhost",
    api: "openai-chat",
    apiKey: "key-{label}",
    models: [{{ id: "oau-model", name: "Oau model", contextWindow: 1000, maxTokens: 100, cost: {{ input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }} }}],
    oauth: {{
      name: "Oau {label}",
      isSubscription: false,
      login: async () => ({{ refresh: "r-{label}", access: "{label}", expires: 9 }}),
      refreshToken: async (credentials) => ({{ refresh: "r2-{label}", access: "refreshed-{label}", expires: 10 }}),
      getApiKey: (credentials) => "derived-" + credentials.access,
    }},
  }});
}}
"#
    )
}

fn write_oauth_extension(dir: &TempDir, label: &str) -> PathBuf {
    let path = dir.path().join(format!("oauth-{label}.ts"));
    std::fs::write(&path, oauth_extension_source(label)).expect("write oauth extension");
    path
}

async fn load_oauth_runtime(path: &Path) -> ExtensionHandle {
    let runtime = ExtensionHandle::start(extension_config(
        path.parent().expect("extension has a parent directory"),
    ))
    .expect("extension handle");
    let result = runtime
        .load(&[path.to_path_buf()])
        .await
        .expect("load oauth extension");
    assert!(
        result.errors.is_empty(),
        "oauth extension load errors: {:?}",
        result.errors
    );
    runtime
}

async fn start_oauth_runtime(dir: &TempDir, label: &str) -> ExtensionHandle {
    load_oauth_runtime(&write_oauth_extension(dir, label)).await
}

/// An ephemeral session mirroring the subagent child shape: it owns its own
/// JS runtime and shares the parent's `Arc<ProviderRegistry>`.
fn oauth_session(
    registry: &Arc<ProviderRegistry>,
    runtime: ExtensionHandle,
    cwd: &Path,
    sessions: &Path,
) -> AgentSession {
    std::fs::create_dir_all(sessions).expect("create session dir");
    let cwd_text = cwd.to_string_lossy().into_owned();
    let mut config = agent_config_with_registry(
        cwd,
        SessionManager::create(&cwd_text, sessions).expect("session"),
        Arc::clone(registry),
    );
    config.session = None;
    config.extensions = Some(runtime);
    AgentSession::new_with_plugins(config, Vec::new())
}

/// Headless login interaction: only `notify` is required, and the fixture's
/// login callback prompts for nothing.
struct NoopLoginInteraction;

impl AuthInteraction for NoopLoginInteraction {
    fn prompt<'a>(
        &'a self,
        _prompt: AuthPrompt,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<String>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn notify(&self, _event: AuthEvent) {}
}

/// Run the installed provider's OAuth login and return the credential's
/// `access` value, which the fixture sets to the owning runtime's label.
async fn oauth_login_label(registry: &ProviderRegistry, provider_id: &str) -> String {
    let provider = registered_provider(registry, provider_id);
    oauth_provider_label(&provider).await
}

/// Run one concrete provider allocation's OAuth login (SNAP8: lets a test
/// assert which session's overlay entry a scoped lookup resolved without
/// going through the process-shared dynamic map).
async fn oauth_provider_label(provider: &Arc<dyn Provider>) -> String {
    let flow = provider.oauth_flow().expect("oauth flow installed");
    flow.login(&NoopLoginInteraction, &LoginOptions::default())
        .await
        .expect("oauth login")
        .access
}

/// Registry with two quiet providers (3 + 2 models) so a catalog build is
/// measurable by its serialization count.
fn test_registry() -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(CatalogProvider::new("alpha", 3)));
    registry.set_provider(Arc::new(CatalogProvider::new("beta", 2)));
    Arc::new(registry)
}

/// Write the models.json that grants `provider` a literal api key. Callers
/// pass different provider/key lengths so successive writes always differ in
/// size as well as mtime (the two stamps the cache compares).
fn write_models_json(agent_dir: &Path, provider: &str, key: &str) {
    let contents = format!("{{\"providers\":{{\"{provider}\":{{\"apiKey\":\"{key}\"}}}}}}\n");
    std::fs::write(agent_dir.join("models.json"), contents).expect("write models.json");
}

/// Write a models.json whose apiKey is a `$ENV` template.
fn write_models_json_template(agent_dir: &Path, provider: &str, template: &str) {
    let contents = format!("{{\"providers\":{{\"{provider}\":{{\"apiKey\":\"{template}\"}}}}}}\n");
    std::fs::write(agent_dir.join("models.json"), contents).expect("write models.json");
}

/// `ctx.modelRegistry.hasConfiguredAuth` through the live JS host function.
async fn has_authenticated_provider(handle: &ExtensionHandle, provider: &str) -> bool {
    handle
        .eval_json(&format!(
            "JSON.stringify(__pir_host.hasAuthenticatedProvider({provider:?}))"
        ))
        .await
        .expect("read hasAuthenticatedProvider")
        .as_bool()
        .expect("hasAuthenticatedProvider returns a bool")
}

fn extension_config(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

fn agent_config(cwd: &Path, session: SessionManager) -> AgentSessionConfig {
    agent_config_with_registry(cwd, session, Arc::new(ProviderRegistry::default()))
}

fn agent_config_with_registry(
    cwd: &Path,
    session: SessionManager,
    provider_registry: Arc<ProviderRegistry>,
) -> AgentSessionConfig {
    AgentSessionConfig {
        performance: Default::default(),
        provider_registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 1,
        system_prompt: None,
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

/// Inline plugin recording every `on_session_context` push: a cache hit must
/// not send the (identical) snapshot through the plugin fan-out again.
struct SnapshotRecorder {
    info: PluginInfo,
    pushes: Arc<Mutex<Vec<SessionContextSnapshot>>>,
}

impl SnapshotRecorder {
    fn new() -> (Self, Arc<Mutex<Vec<SessionContextSnapshot>>>) {
        let pushes = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                info: PluginInfo::inline("snapshot-recorder"),
                pushes: Arc::clone(&pushes),
            },
            pushes,
        )
    }
}

impl AgentPlugin for SnapshotRecorder {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn on_session_context(&self, snapshot: &SessionContextSnapshot) {
        self.pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(snapshot.clone());
    }
}

async fn snapshot_json(handle: &ExtensionHandle) -> serde_json::Value {
    handle
        .eval_json("__pir_host.getSessionManagerJson()")
        .await
        .expect("read the pushed snapshot")
}

/// The pushed `{models, providers}` catalog as the JS `ctx.modelRegistry`
/// reads it.
async fn snapshot_catalog(handle: &ExtensionHandle) -> serde_json::Value {
    handle
        .eval_json("__pir_host.getModelsJson()")
        .await
        .expect("read the pushed model catalog")
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn unchanged_push_reuses_the_previous_snapshot() {
    let _serial = env_lock();
    let temp = TempDir::new("reuse");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("first turn"))
        .expect("append user");
    manager
        .append_message(assistant_message("first reply"))
        .expect("append assistant");

    let (recorder, pushes) = SnapshotRecorder::new();
    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
    let mut config = agent_config(temp.path(), manager);
    config.extensions = Some(handle.clone());
    let recorder: Arc<dyn AgentPlugin> = Arc::new(recorder);
    let agent = AgentSession::new_with_plugins(config, vec![recorder]);

    reset_session_list_file_reads();
    assert!(
        agent.push_session_context_for_test().await,
        "the first push has no previous snapshot and must build"
    );
    let first = snapshot_json(&handle).await;
    assert_eq!(
        pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1,
        "the first push reaches the plugin fan-out"
    );
    assert_eq!(
        first["entries"].as_array().map(Vec::len),
        Some(2),
        "the snapshot carries the projected context entries: {first}"
    );
    assert_eq!(
        session_list_file_reads(),
        1,
        "the build lists the one session file exactly once"
    );

    // No session mutation between the two pushes: the runtime already holds
    // this exact snapshot, so the second push must be a no-op.
    reset_session_list_file_reads();
    assert!(
        !agent.push_session_context_for_test().await,
        "an unchanged push must not rebuild"
    );
    let second = snapshot_json(&handle).await;
    assert_eq!(first, second, "the runtime snapshot is unchanged");
    assert_eq!(
        pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1,
        "a cache hit must not fan the identical snapshot out again"
    );
    assert_eq!(
        session_list_file_reads(),
        0,
        "a skipped push must not re-read the session directory"
    );

    // A new entry invalidates the fingerprint.
    let manager = agent.session_manager().expect("persisted session");
    manager
        .lock()
        .await
        .append_message(user_message("second turn"))
        .expect("append user");
    assert!(
        agent.push_session_context_for_test().await,
        "a new session entry must invalidate the cache"
    );
    let third = snapshot_json(&handle).await;
    assert_ne!(first, third, "the new entry must reach the snapshot");
    assert_eq!(
        third["entries"].as_array().map(Vec::len),
        Some(3),
        "the snapshot carries the new projected entry: {third}"
    );
    assert_eq!(
        pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        2,
        "the rebuilt snapshot is pushed to the plugin fan-out"
    );
}

/// `SessionContextSnapshot.context_tokens` is the LAST usage record (upstream
/// `getContextUsage`), not the lifetime `self.tokens` accumulator: two
/// assistant turns reporting 100 and 200 must snapshot 200, never 300, and a
/// later turn must move the value without the fingerprint hiding it.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn context_tokens_reports_last_usage_not_the_lifetime_total() {
    let _serial = env_lock();
    let temp = TempDir::new("last-usage");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("first turn"))
        .expect("append user");
    manager
        .append_message(assistant_message_with_usage("first reply", 100))
        .expect("append assistant");
    manager
        .append_message(user_message("second turn"))
        .expect("append user");
    manager
        .append_message(assistant_message_with_usage("second reply", 200))
        .expect("append assistant");

    let (recorder, pushes) = SnapshotRecorder::new();
    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
    let mut config = agent_config(temp.path(), manager);
    config.extensions = Some(handle.clone());
    let recorder: Arc<dyn AgentPlugin> = Arc::new(recorder);
    let agent = AgentSession::new_with_plugins(config, vec![recorder]);

    assert!(
        agent.push_session_context_for_test().await,
        "the first push has no previous snapshot and must build"
    );
    let last_tokens = {
        let pushes = pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pushes
            .last()
            .expect("the plugin fan-out received the snapshot")
            .context_tokens
    };
    assert_eq!(
        last_tokens,
        Some(200),
        "context_tokens must be the newest usage (200), not the cumulative 300"
    );
    let snapshot = snapshot_json(&handle).await;
    assert_eq!(
        snapshot["contextTokens"],
        serde_json::json!(200),
        "the JS-visible snapshot reports the last usage: {snapshot}"
    );

    // A third assistant turn moves the value; the fingerprint must not hide
    // the change (an unchanged push would leave the stale 200 in place).
    let manager = agent.session_manager().expect("persisted session");
    manager
        .lock()
        .await
        .append_message(assistant_message_with_usage("third reply", 350))
        .expect("append assistant");
    assert!(
        agent.push_session_context_for_test().await,
        "a new usage value must rebuild the snapshot"
    );
    let last_tokens = {
        let pushes = pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pushes
            .last()
            .expect("the plugin fan-out received the rebuilt snapshot")
            .context_tokens
    };
    assert_eq!(
        last_tokens,
        Some(350),
        "the rebuilt snapshot tracks the newest usage record"
    );
    assert_eq!(
        pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        2,
        "exactly one rebuild reached the plugin fan-out"
    );
}

/// P3-01 performance regression: the cache HIT path must not call
/// `context_usage()` (an O(entries) projection-metadata walk). The call-count
/// seam is per-thread and current-thread-runtime scoped, so the count is exact
/// and repeatable — no timing or 10k-entry fixture needed. Moving
/// `context_usage()` back before the fingerprint check makes the second push
/// count 2 instead of 1 and fails here.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn cache_hit_does_not_walk_context_usage() {
    let _serial = env_lock();
    let temp = TempDir::new("usage-calls");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("first turn"))
        .expect("append user");
    manager
        .append_message(assistant_message("first reply"))
        .expect("append assistant");

    let (recorder, _pushes) = SnapshotRecorder::new();
    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
    let mut config = agent_config(temp.path(), manager);
    config.extensions = Some(handle);
    let recorder: Arc<dyn AgentPlugin> = Arc::new(recorder);
    let agent = AgentSession::new_with_plugins(config, vec![recorder]);

    agent.reset_session_context_usage_calls_for_test();
    assert!(
        agent.push_session_context_for_test().await,
        "the first push has no previous snapshot and must build"
    );
    assert_eq!(
        agent.session_context_usage_calls_for_test(),
        1,
        "a cache miss pays exactly one context_usage() walk"
    );

    assert!(
        !agent.push_session_context_for_test().await,
        "an unchanged push must hit the fingerprint cache"
    );
    assert_eq!(
        agent.session_context_usage_calls_for_test(),
        1,
        "a cache hit must not call context_usage() (P3-01: the hit path stays O(1))"
    );

    // A session append moves entry_count/last_entry_id: the next push is a
    // miss and pays one more walk (the cache did not freeze the value).
    agent
        .session_manager()
        .expect("persisted session")
        .lock()
        .await
        .append_message(user_message("second turn"))
        .expect("append user");
    assert!(
        agent.push_session_context_for_test().await,
        "a new session entry must invalidate the cache"
    );
    assert_eq!(
        agent.session_context_usage_calls_for_test(),
        2,
        "a cache miss pays one context_usage() walk again"
    );
}

/// R1-L1-008: the model catalog / authenticated-provider push is cached by
/// registry identity + credential-file stamps. The first push builds it once
/// (one `to_value` per model, one `load_pi_environment`); an unchanged second
/// push — a session-snapshot cache hit — must perform ZERO serializations and
/// ZERO environment loads. Editing `models.json` invalidates the catalog cache
/// even though the session-derived snapshot itself is unchanged: the rebuilt
/// catalog carries the new authenticated provider list.
#[tokio::test(flavor = "current_thread")]
async fn model_catalog_push_is_cached_until_the_auth_environment_changes() {
    let temp = TempDir::new("catalog-cache");
    let agent_dir = TempDir::new("catalog-cache-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    write_models_json(agent_dir.path(), "alpha", "key-alpha");

    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("first turn"))
        .expect("append user");

    let registry = test_registry();
    let (recorder, _pushes) = SnapshotRecorder::new();
    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
    let mut config = agent_config_with_registry(temp.path(), manager, Arc::clone(&registry));
    config.extensions = Some(handle.clone());
    let recorder: Arc<dyn AgentPlugin> = Arc::new(recorder);
    let agent = AgentSession::new_with_plugins(config, vec![recorder]);

    assert!(
        agent.push_session_context_for_test().await,
        "the first push has no previous snapshot and must build"
    );
    assert_eq!(
        agent.model_catalog_push_stats(),
        (1, 5, 1),
        "one catalog push carries alpha's 3 + beta's 2 serialized models and \
         loads the credential environment once"
    );
    let catalog = snapshot_catalog(&handle).await;
    assert_eq!(
        catalog["models"].as_array().map(Vec::len),
        Some(5),
        "the runtime holds the full serialized catalog: {catalog}"
    );
    assert!(
        !has_authenticated_provider(&handle, "beta").await,
        "models.json grants alpha only, so beta is unauthenticated"
    );

    // Nothing moved: the second push is a session-snapshot cache hit, and the
    // catalog must not be rebuilt/serialized/reread.
    agent.reset_model_catalog_push_stats();
    assert!(
        !agent.push_session_context_for_test().await,
        "an unchanged push must hit the fingerprint cache"
    );
    assert_eq!(
        agent.model_catalog_push_stats(),
        (0, 0, 0),
        "a cache hit must skip the catalog rebuild, every model-registry push \
         and the credential-environment load"
    );

    // A credential-file edit invalidates the catalog cache even though the
    // session-derived snapshot still matches. The project settings path is
    // unchanged; only models.json grants a different provider now.
    write_models_json(agent_dir.path(), "beta", "key-beta-longer");
    agent.reset_model_catalog_push_stats();
    assert!(
        !agent.push_session_context_for_test().await,
        "the session-derived snapshot is still unchanged"
    );
    assert_eq!(
        agent.model_catalog_push_stats(),
        (1, 5, 1),
        "the models.json change forces exactly one catalog rebuild and push"
    );
    assert!(
        has_authenticated_provider(&handle, "beta").await,
        "the rebuilt catalog carries the new authenticated provider list"
    );
    assert!(
        !has_authenticated_provider(&handle, "alpha").await,
        "alpha's credential was removed by the same edit"
    );

    // The fresh stamp is retained: the next unchanged push is a hit again.
    agent.reset_model_catalog_push_stats();
    assert!(!agent.push_session_context_for_test().await);
    assert_eq!(
        agent.model_catalog_push_stats(),
        (0, 0, 0),
        "the rebuilt stamp is cached for subsequent pushes"
    );
}

/// P3-新-3: a transient `load_pi_environment` failure must not be cached as a
/// permanent empty authenticated-provider list. The first failed push builds
/// with the permissive-empty list and keeps no catalog stamp, so the next
/// push retries the load instead of reporting a cache hit.
#[tokio::test(flavor = "current_thread")]
async fn model_catalog_push_retries_after_a_transient_credential_load_failure() {
    let temp = TempDir::new("catalog-load-retry");
    let agent_dir = TempDir::new("catalog-load-retry-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    std::fs::write(agent_dir.path().join("models.json"), "{ not valid json\n")
        .expect("write invalid models.json");

    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("first turn"))
        .expect("append user");

    let registry = test_registry();
    let (recorder, _pushes) = SnapshotRecorder::new();
    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
    let mut config = agent_config_with_registry(temp.path(), manager, Arc::clone(&registry));
    config.extensions = Some(handle.clone());
    let recorder: Arc<dyn AgentPlugin> = Arc::new(recorder);
    let agent = AgentSession::new_with_plugins(config, vec![recorder]);

    assert!(agent.push_session_context_for_test().await);
    assert_eq!(
        agent.model_catalog_push_stats(),
        (1, 5, 1),
        "the failed load still builds/pushes the catalog and counts the load"
    );

    agent.reset_model_catalog_push_stats();
    assert!(
        !agent.push_session_context_for_test().await,
        "the session-derived snapshot is unchanged"
    );
    assert_eq!(
        agent.model_catalog_push_stats(),
        (1, 5, 1),
        "a failed credential load is retried, not cached as a permanent empty list"
    );

    // Recovery: a valid file is picked up by the retry.
    write_models_json(agent_dir.path(), "alpha", "key-alpha");
    agent.reset_model_catalog_push_stats();
    assert!(!agent.push_session_context_for_test().await);
    assert_eq!(agent.model_catalog_push_stats(), (1, 5, 1));
    assert!(
        has_authenticated_provider(&handle, "alpha").await,
        "the retried load carries the recovered credential list"
    );
}

/// P3-新-2: an equal-length rewrite whose mtime is restored to the previous
/// value must still invalidate the catalog — the fingerprint carries a
/// content hash, not only `(mtime, size)`. The old implementation went green
/// here only because the fixture deliberately changed the file length.
#[tokio::test(flavor = "current_thread")]
async fn model_catalog_push_detects_same_size_same_mtime_rewrite() {
    let temp = TempDir::new("catalog-same-stamp");
    let agent_dir = TempDir::new("catalog-same-stamp-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    write_models_json(agent_dir.path(), "alpha", "key-alpha");
    let path = agent_dir.path().join("models.json");
    let before = std::fs::metadata(&path)
        .expect("metadata")
        .modified()
        .expect("mtime");
    let before_len = std::fs::metadata(&path).expect("metadata").len();

    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("first turn"))
        .expect("append user");

    let registry = test_registry();
    let (recorder, _pushes) = SnapshotRecorder::new();
    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
    let mut config = agent_config_with_registry(temp.path(), manager, Arc::clone(&registry));
    config.extensions = Some(handle.clone());
    let recorder: Arc<dyn AgentPlugin> = Arc::new(recorder);
    let agent = AgentSession::new_with_plugins(config, vec![recorder]);

    assert!(agent.push_session_context_for_test().await);
    assert!(has_authenticated_provider(&handle, "alpha").await);

    // "alpha"/"key-alpha" -> "bravo"/"key-bravo": same byte length.
    write_models_json(agent_dir.path(), "bravo", "key-bravo");
    {
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open models.json");
        file.set_times(std::fs::FileTimes::new().set_modified(before))
            .expect("restore mtime");
    }
    let after_len = std::fs::metadata(&path).expect("metadata").len();
    assert_eq!(
        before_len, after_len,
        "test setup requires an equal-length rewrite"
    );
    assert_eq!(
        std::fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime"),
        before,
        "test setup requires the mtime stamp to be unchanged"
    );

    agent.reset_model_catalog_push_stats();
    assert!(
        !agent.push_session_context_for_test().await,
        "the session-derived snapshot is unchanged"
    );
    assert_eq!(
        agent.model_catalog_push_stats(),
        (1, 5, 1),
        "a same-size/same-mtime rewrite must rebuild from the content hash"
    );
    assert!(
        has_authenticated_provider(&handle, "bravo").await,
        "the rewritten credential must reach the runtime"
    );
    assert!(
        !has_authenticated_provider(&handle, "alpha").await,
        "the old credential must not survive the rewrite"
    );
}

/// P3-新-1: `models.json` `$ENV` templates resolve against the process
/// environment, so a value change without a file edit must invalidate the
/// catalog cache. Removing the referenced key flips `alpha` from
/// authenticated to unauthenticated; a stamp-only fingerprint stays green on
/// the stale list.
#[tokio::test(flavor = "current_thread")]
async fn model_catalog_push_detects_an_auth_env_value_change() {
    const KEY_VAR: &str = "PIR_SNAP2_TEST_API_KEY";
    let temp = TempDir::new("catalog-env-change");
    let agent_dir = TempDir::new("catalog-env-change-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let _key = EnvVarOverride::set(KEY_VAR, "key-one");
    write_models_json_template(agent_dir.path(), "alpha", "$PIR_SNAP2_TEST_API_KEY");

    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("first turn"))
        .expect("append user");

    let registry = test_registry();
    let (recorder, _pushes) = SnapshotRecorder::new();
    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
    let mut config = agent_config_with_registry(temp.path(), manager, Arc::clone(&registry));
    config.extensions = Some(handle.clone());
    let recorder: Arc<dyn AgentPlugin> = Arc::new(recorder);
    let agent = AgentSession::new_with_plugins(config, vec![recorder]);

    assert!(agent.push_session_context_for_test().await);
    assert!(
        has_authenticated_provider(&handle, "alpha").await,
        "the $ENV template resolves alpha's key"
    );

    // No file edit: only the referenced environment value disappears.
    std::env::remove_var(KEY_VAR);
    agent.reset_model_catalog_push_stats();
    assert!(
        !agent.push_session_context_for_test().await,
        "the session-derived snapshot is unchanged"
    );
    assert_eq!(
        agent.model_catalog_push_stats(),
        (1, 5, 1),
        "an auth-relevant env change must rebuild the catalog"
    );
    assert!(
        !has_authenticated_provider(&handle, "alpha").await,
        "a stamp-only fingerprint would keep the stale authenticated list"
    );
}

/// R1-L1-008: a registry mutation (a virtual-model registration changes the
/// serialized catalog and the virtual-model fingerprint) invalidates the
/// catalog cache without touching the session-derived snapshot.
#[tokio::test(flavor = "current_thread")]
async fn model_catalog_push_rebuilds_after_a_registry_registration() {
    let temp = TempDir::new("catalog-registry");
    let agent_dir = TempDir::new("catalog-registry-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    write_models_json(agent_dir.path(), "alpha", "key-alpha");

    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("first turn"))
        .expect("append user");

    let registry = test_registry();
    let (recorder, _pushes) = SnapshotRecorder::new();
    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
    let mut config = agent_config_with_registry(temp.path(), manager, Arc::clone(&registry));
    config.extensions = Some(handle.clone());
    let recorder: Arc<dyn AgentPlugin> = Arc::new(recorder);
    let agent = AgentSession::new_with_plugins(config, vec![recorder]);

    assert!(agent.push_session_context_for_test().await);
    assert_eq!(agent.model_catalog_push_stats(), (1, 5, 1));
    agent.reset_model_catalog_push_stats();
    assert!(!agent.push_session_context_for_test().await);
    assert_eq!(
        agent.model_catalog_push_stats(),
        (0, 0, 0),
        "baseline cache hit performs no catalog work"
    );

    registry
        .register_virtual_model(VirtualModelRegistration {
            id: "alpha-virtual".to_owned(),
            provider: "alpha".to_owned(),
            name: Some("Alpha virtual".to_owned()),
            physical: PhysicalModelRef {
                provider: "alpha".to_owned(),
                id: "alpha-0".to_owned(),
            },
            reasoning: None,
            system_prompt: None,
            model_override: None,
            source: "host".to_owned(),
        })
        .expect("register virtual model");

    assert!(
        !agent.push_session_context_for_test().await,
        "the session-derived snapshot is unchanged"
    );
    assert_eq!(
        agent.model_catalog_push_stats(),
        (1, 6, 1),
        "the registry change forces one rebuild with the new 6-model catalog"
    );
    let catalog = snapshot_catalog(&handle).await;
    assert_eq!(catalog["models"].as_array().map(Vec::len), Some(6));
    assert!(
        catalog["models"]
            .as_array()
            .is_some_and(|models| models.iter().any(|model| model["id"] == "alpha-virtual")),
        "the virtual model reaches ctx.modelRegistry: {catalog}"
    );
}

/// P2-新-1: a session with a `pi.registerProvider` definition re-syncs that
/// provider on every prompt. The re-sync must short-circuit when the definition
/// and installed model content are unchanged (reusing the dynamic provider
/// `Arc`), and the catalog fingerprint must key on the plugin definition
/// (id + config hash + model content) instead of the address. Either half
/// failing rebuilds the whole catalog every prompt (SNAP1 reported
/// `(1, 6, 1)` on the second push, not `(0, 0, 0)`).
///
/// The counters live at the `update_model_registry` sink, so this test fails
/// on the SNAP1 implementation, not merely on a missing test seam.
#[tokio::test(flavor = "current_thread")]
async fn model_catalog_push_is_cached_across_plugin_provider_resyncs() {
    let temp = TempDir::new("catalog-plugin-provider");
    let agent_dir = TempDir::new("catalog-plugin-provider-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    write_models_json(agent_dir.path(), "alpha", "key-alpha");

    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("first turn"))
        .expect("append user");

    let registry = test_registry();
    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
    let mut config = agent_config_with_registry(temp.path(), manager, Arc::clone(&registry));
    config.extensions = Some(handle.clone());
    let provider_plugin: Arc<dyn AgentPlugin> = Arc::new(GammaProviderPlugin::new());
    let agent = AgentSession::new_with_plugins(config, vec![provider_plugin]);

    // The prompt loop's pre-run sync registers `gamma` before the push.
    agent.sync_plugin_providers_for_test().await;
    agent.reset_model_catalog_push_stats();
    assert!(
        agent.push_session_context_for_test().await,
        "the first push has no previous snapshot and must build"
    );
    assert_eq!(
        agent.model_catalog_push_stats(),
        (1, 6, 1),
        "one build pushes alpha's 3 + beta's 2 + gamma's 1 model"
    );
    let catalog = snapshot_catalog(&handle).await;
    assert_eq!(
        catalog["models"].as_array().map(Vec::len),
        Some(6),
        "the plugin provider's model reaches ctx.modelRegistry: {catalog}"
    );

    // Keep the first dynamic `Arc` alive so a replacement cannot reuse its
    // address: the old Arc-identity fingerprint then fails deterministically
    // (not via an ABA coincidence).
    let old_dynamic = registry
        .providers()
        .iter()
        .find(|provider| provider.id() == "gamma")
        .cloned()
        .expect("gamma is registered");
    // A second per-prompt sync with the identical definition must short-circuit
    // and reuse the existing dynamic provider `Arc`, not rebuild it.
    agent.reset_model_catalog_push_stats();
    agent.sync_plugin_providers_for_test().await;
    let new_dynamic = registry
        .providers()
        .iter()
        .find(|provider| provider.id() == "gamma")
        .cloned()
        .expect("gamma remains registered");
    assert!(
        Arc::ptr_eq(&old_dynamic, &new_dynamic),
        "an unchanged provider re-sync must reuse the existing dynamic Arc (P2-新-1)"
    );
    assert!(
        !agent.push_session_context_for_test().await,
        "the session-derived snapshot is unchanged"
    );
    assert_eq!(
        agent.model_catalog_push_stats(),
        (0, 0, 0),
        "an unchanged provider re-sync must not rebuild or re-push the catalog"
    );
    let catalog = snapshot_catalog(&handle).await;
    assert_eq!(
        catalog["models"].as_array().map(Vec::len),
        Some(6),
        "the cached catalog still carries the plugin provider: {catalog}"
    );
}

/// SNAP3 P2-1: a subagent child shares the parent's `Arc<ProviderRegistry>`
/// but owns a fresh, empty `extension_api_keys` map. The provider-sync stamp
/// must therefore be keyed per SESSION as well as per registry scope: a
/// registry-only stamp made the child's first sync short-circuit on the
/// parent's stamp and silently drop `pi.registerProvider({apiKey})`, so the
/// child's requests carried no plugin key. The old implementation fails the
/// child assertion because its sync returns before installing any key.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn child_session_sharing_registry_installs_its_own_extension_api_keys() {
    let temp = TempDir::new("catalog-child-provider");
    let agent_dir = TempDir::new("catalog-child-provider-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let cwd = temp.path().to_string_lossy().into_owned();
    let registry = test_registry();

    let mut parent_config = agent_config_with_registry(
        temp.path(),
        SessionManager::create(&cwd, temp.path()).expect("parent session"),
        Arc::clone(&registry),
    );
    parent_config.session = None;
    let parent =
        AgentSession::new_with_plugins(parent_config, vec![Arc::new(GammaProviderPlugin::new())]);
    parent.sync_plugin_providers_for_test().await;
    assert!(
        parent.extension_api_key_installed_for_test("gamma"),
        "the parent installs its plugin provider key"
    );

    // The child's construction mirrors `plugins/subagent/child.rs`: it shares
    // the parent's provider registry (same refresh scope) but has its own
    // extension key map.
    let child_sessions = temp.path().join("child-sessions");
    std::fs::create_dir_all(&child_sessions).expect("create child session dir");
    let mut child_config = agent_config_with_registry(
        temp.path(),
        SessionManager::create(&cwd, &child_sessions).expect("child session"),
        Arc::clone(&registry),
    );
    child_config.session = None;
    let child =
        AgentSession::new_with_plugins(child_config, vec![Arc::new(GammaProviderPlugin::new())]);
    child.sync_plugin_providers_for_test().await;

    assert!(
        child.extension_api_key_installed_for_test("gamma"),
        "a child sharing the parent registry must still install its own \
         extension API key on its first sync (SNAP2's registry-only stamp \
         skipped it)"
    );
    assert!(
        parent.extension_api_key_installed_for_test("gamma"),
        "the parent's key must survive the child sync"
    );
}

/// SNAP4 P2-新-1: a parent and two subagent-like children share ONE
/// `Arc<ProviderRegistry>`. Each session owns a different JS runtime and the
/// same `pi.registerProvider({oauth, apiKey})` definition, so the shared
/// dynamic map can only hold one provider/wrapper per id. A session's fast
/// path must verify that the exact allocations it installed are still the
/// ones the registry resolves; otherwise a sibling's sync silently leaves the
/// parent/child using another session's `OAuthFlowOverlay` and extension API
/// key. The overlay is also weak-bound: the registry must not keep a dropped
/// child's extension runtime alive.
///
/// The old SNAP3 implementation fails twice here: after child2's sync,
/// child1's re-sync short-circuits on definitions/models (both identical) and
/// its next `flow.login()` executes child2's JS callback; the runtime token
/// also stays alive because `JsOAuthFlow` holds a strong `ExtensionHandle`.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn shared_registry_child_oauth_overlays_and_keys_are_session_bound() {
    let temp = TempDir::new("catalog-child-oauth");
    let agent_dir = TempDir::new("catalog-child-oauth-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let registry = test_registry();

    let parent_runtime = start_oauth_runtime(&temp, "parent").await;
    let parent = oauth_session(
        &registry,
        parent_runtime.clone(),
        temp.path(),
        &temp.path().join("parent-sessions"),
    );
    parent
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(
        parent.extension_api_key_for_test("oauprov").as_deref(),
        Some("key-parent"),
        "the parent installs its own plugin API key"
    );
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "parent",
        "the parent's OAuth overlay dispatches to the parent runtime"
    );

    let child1_runtime = start_oauth_runtime(&temp, "child1").await;
    let child1 = oauth_session(
        &registry,
        child1_runtime.clone(),
        temp.path(),
        &temp.path().join("child1-sessions"),
    );
    child1
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(
        child1.extension_api_key_for_test("oauprov").as_deref(),
        Some("key-child1"),
        "child1 installs its own plugin API key"
    );
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "child1",
        "child1's OAuth overlay dispatches to child1's runtime"
    );

    let child2_runtime = start_oauth_runtime(&temp, "child2").await;
    let child2 = oauth_session(
        &registry,
        child2_runtime.clone(),
        temp.path(),
        &temp.path().join("child2-sessions"),
    );
    child2
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(
        child2.extension_api_key_for_test("oauprov").as_deref(),
        Some("key-child2"),
        "child2 installs its own plugin API key"
    );
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "child2",
        "child2's OAuth overlay dispatches to child2's runtime"
    );

    // Child1's next prompt re-syncs after child2 replaced the shared dynamic
    // entry. Its stamp (same definitions/models) must NOT short-circuit: the
    // installed `Arc` identity changed, so child1 re-installs its own
    // overlay/key before any request uses it.
    child1.sync_plugin_providers_for_test().await;
    assert_eq!(
        child1.extension_api_key_for_test("oauprov").as_deref(),
        Some("key-child1"),
        "child1's key is still its own after the sibling sync"
    );
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "child1",
        "child1's re-sync must restore child1's OAuth overlay; SNAP3's \
         definitions/models-only stamp kept child2's flow"
    );

    // Same for the parent: its next prompt must take the child's wrapper back
    // to the parent's runtime instead of reusing the child's handle.
    parent.sync_plugin_providers_for_test().await;
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "parent",
        "the parent's re-sync must restore the parent's OAuth overlay; SNAP3's \
         fast path reused the child's handle"
    );

    // Re-bind the shared registry to child1, then drop child1 and every
    // strong handle it owns. A strong `JsOAuthFlow` in the registry would keep
    // the actor (and its QuickJS runtime) alive, so this login would still
    // execute child1's JS callback; the weak binding must fail cleanly.
    child1.sync_plugin_providers_for_test().await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "child1");
    drop(child1);
    drop(child1_runtime);
    let provider = registered_provider(&registry, "oauprov");
    let flow = provider
        .oauth_flow()
        .expect("child1's overlay is still the registry entry");
    assert!(
        flow.login(&NoopLoginInteraction, &LoginOptions::default())
            .await
            .is_err(),
        "the shared registry's child OAuth overlay must not pin the child \
         extension runtime after the child session drops; a strong \
         `JsOAuthFlow` handle would still execute child1's login callback"
    );

    // Keep the other sessions alive through the liveness assertion so the
    // registry entry inspected above is the one this test installed.
    drop(child2);
    drop(parent);
}

/// SNAP6 P1-1: a subagent child shares the parent's `Arc<ProviderRegistry>`,
/// so its provider sync installs a child-owned `oauprov` overlay into the
/// shared dynamic map. When the child run ends, `run_child`'s cleanup guard
/// (the same object
/// [`AgentSession::provider_sync_cleanup_guard_for_test`] returns) must
/// RESTORE the parent's previous allocation, not merely delete the child's
/// entry: the parent keeps running the same prompt after the subagent tool
/// returns and `sync_plugin_providers` only runs at `run_prompt` start, so a
/// deleted entry makes the parent's next model request fail with
/// `Unknown provider`.
///
/// This is the drop side of the guard, not a direct call to the registry:
/// dropping it runs the exact synchronous cleanup `Drop for
/// ProviderSyncCleanupGuard` runs at child teardown.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn child_session_cleanup_restores_the_parent_shared_oauth_provider() {
    let temp = TempDir::new("catalog-child-cleanup");
    let agent_dir = TempDir::new("catalog-child-cleanup-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let registry = test_registry();

    let parent_runtime = start_oauth_runtime(&temp, "parent").await;
    let parent = oauth_session(
        &registry,
        parent_runtime.clone(),
        temp.path(),
        &temp.path().join("parent-sessions"),
    );
    parent
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "parent");
    // Keep only a Weak: the cleanup must itself keep the displaced parent
    // provider alive (a Weak-only `previous` would die when the child's sync
    // replaces the map entry, and this test would fail with a dead upgrade).
    let parent_provider = Arc::downgrade(&registered_provider(&registry, "oauprov"));

    // Spawn a REAL ChildRuntime from the parent's recorded path plan: this is
    // the same seam `run_child` uses before arming the cleanup guard. The
    // child runtime loads its own copy of the extension, so the child
    // session's sync installs a child-owned overlay in the shared registry.
    let child_path = write_oauth_extension(&temp, "child");
    parent_runtime.set_path_plan(vec![child_path]);
    let catalog = pir::agent_session::ModelCatalog::from_registry(&registry);
    let child_runtime =
        pir::plugins::subagent::spawn_child_runtime(&parent_runtime, temp.path(), &catalog)
            .await
            .expect("child runtime");
    let child = oauth_session(
        &registry,
        child_runtime.handle().clone(),
        temp.path(),
        &temp.path().join("child-sessions"),
    );
    child
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "child",
        "the child's sync owns the shared dynamic entry while it runs"
    );

    // Child teardown (normal end of `run_child` or a dropped run future).
    drop(child.provider_sync_cleanup_guard_for_test());

    // The parent's own allocation must be back in the shared map. This is
    // the exact provider the parent's next `stream_resolving_auth` resolves;
    // deleting the child entry without restoring it left this as `None`.
    let restored = registered_provider(&registry, "oauprov");
    let expected = parent_provider
        .upgrade()
        .expect("the child stamp must keep the displaced parent provider alive");
    assert!(
        Arc::ptr_eq(&restored, &expected),
        "child teardown must restore the parent's previous provider allocation, \
         not delete the id"
    );
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "parent",
        "the restored provider must dispatch OAuth to the parent runtime"
    );

    // The parent's NEXT model request in the same prompt goes through
    // `provider_box`. A deleted entry emits a synchronous `Unknown provider`
    // error event; the restored entry must not.
    assert!(
        registry.provider_box_for_test("oauprov").is_some(),
        "the parent's next request must resolve a provider"
    );
    let model = restored
        .get_models()
        .into_iter()
        .next()
        .expect("the parent provider exposes its model");
    let stream = registry.stream_resolving_auth(
        model,
        pir::ai::types::Context::default(),
        StreamOptions::default(),
    );
    assert!(
        stream.try_next().is_none(),
        "the parent's next model request must not emit `Unknown provider`"
    );

    // The parent's next prompt sync finds its exact allocation still in the
    // map and may reuse it (the wrapper already belongs to the parent).
    parent.sync_plugin_providers_for_test().await;
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "parent",
        "the parent's next sync keeps its own OAuth overlay"
    );

    drop(child_runtime);
    drop(child);
    drop(parent);
}

/// SNAP6 P1-1: with no previous owner for the id, child teardown still has
/// to delete the child-installed entry (restoring `None` == removal).
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn child_session_cleanup_removes_provider_without_previous_owner() {
    let temp = TempDir::new("catalog-child-cleanup-no-previous");
    let agent_dir = TempDir::new("catalog-child-cleanup-no-previous-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let registry = test_registry();

    let child_runtime = start_oauth_runtime(&temp, "child").await;
    let child = oauth_session(
        &registry,
        child_runtime.clone(),
        temp.path(),
        &temp.path().join("child-sessions"),
    );
    child
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "child");

    drop(child.provider_sync_cleanup_guard_for_test());

    assert!(
        try_registered_provider(&registry, "oauprov").is_none(),
        "child teardown must remove a child-only provider (no previous owner)"
    );
    // `/login` through the registry is the production path that used to
    // resolve the child's weak flow. With the child entry removed it must
    // report an unknown provider, never execute the child's JS callback or
    // its dead-runtime error.
    let login_error = registry
        .login(
            "oauprov",
            AuthType::OAuth,
            &NoopLoginInteraction,
            &LoginOptions::default(),
        )
        .await
        .expect_err("the child provider entry must be gone");
    assert!(
        matches!(login_error, pir::ai::error::AiError::UnknownProvider(_)),
        "parent /login must not resolve the child's OAuth flow, got: {login_error}"
    );

    drop(child_runtime);
    drop(child);
}

/// SNAP5 P2-1 / SNAP6 P1-1 (identity guard): the child's cleanup must never
/// delete or roll back a provider the parent re-registered after the child's
/// sync. The child stamp's `Weak` points at the replaced allocation; the
/// cleanup's `Arc` identity check has to leave the parent's newer entry alone.
/// A strong `child_installed` handle is kept alive so the check exercises the
/// `Arc::ptr_eq` mismatch, not just a dead `Weak`.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn child_session_cleanup_does_not_remove_parent_replacement() {
    let temp = TempDir::new("catalog-child-cleanup-overwrite");
    let agent_dir = TempDir::new("catalog-child-cleanup-overwrite-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let registry = test_registry();

    let parent_runtime = start_oauth_runtime(&temp, "parent").await;
    let parent = oauth_session(
        &registry,
        parent_runtime.clone(),
        temp.path(),
        &temp.path().join("parent-sessions"),
    );
    parent
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "parent");

    let child_runtime = start_oauth_runtime(&temp, "child").await;
    let child = oauth_session(
        &registry,
        child_runtime.clone(),
        temp.path(),
        &temp.path().join("child-sessions"),
    );
    child
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "child");
    let child_installed = registered_provider(&registry, "oauprov");

    // The parent re-syncs while the child is alive: its entry now owns the
    // shared id and the child's installed `Weak` no longer matches.
    parent.sync_plugin_providers_for_test().await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "parent");
    let parent_replacement = registered_provider(&registry, "oauprov");
    assert!(
        !Arc::ptr_eq(&child_installed, &parent_replacement),
        "the parent's sync must install a fresh allocation"
    );

    drop(child.provider_sync_cleanup_guard_for_test());

    assert!(
        try_registered_provider(&registry, "oauprov").is_some(),
        "child cleanup must not delete a provider the parent re-registered"
    );
    assert!(
        Arc::ptr_eq(
            &registered_provider(&registry, "oauprov"),
            &parent_replacement
        ),
        "child cleanup must not roll the parent's replacement back to the \
         child's (or the parent's older) allocation"
    );
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "parent",
        "the parent's replacement must remain the active OAuth wrapper"
    );

    drop(child_runtime);
    drop(child);
    drop(parent);
}

/// SNAP7 P2-3: parallel siblings install the same provider id in order A, B.
/// A ends first and removes its stamp; B's teardown must walk past A's dead
/// entry and remove the id when the parent never owned it.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn sibling_cleanup_skips_finished_sibling_and_removes_without_parent() {
    let temp = TempDir::new("catalog-sibling-cleanup-no-parent");
    let agent_dir = TempDir::new("catalog-sibling-cleanup-no-parent-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let registry = test_registry();

    let sibling_a_runtime = start_oauth_runtime(&temp, "sibling-a").await;
    let sibling_a = oauth_session(
        &registry,
        sibling_a_runtime.clone(),
        temp.path(),
        &temp.path().join("sibling-a-sessions"),
    );
    sibling_a
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "sibling-a");

    let sibling_b_runtime = start_oauth_runtime(&temp, "sibling-b").await;
    let sibling_b = oauth_session(
        &registry,
        sibling_b_runtime.clone(),
        temp.path(),
        &temp.path().join("sibling-b-sessions"),
    );
    sibling_b
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "sibling-b");

    // A ends first while B is still the current writer. Its cleanup must be a
    // no-op for B's entry and must drop A's ownership record.
    drop(sibling_a.provider_sync_cleanup_guard_for_test());
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "sibling-b",
        "the earlier sibling's cleanup must not touch the current writer"
    );
    // The child session itself is gone before B teardown: both the missing
    // stamp and the dead session weak mark A as an invalid restore target.
    drop(sibling_a);

    // B ends last. A is no longer live, and there is no parent owner, so the
    // restore chain is empty and teardown removes the id.
    drop(sibling_b.provider_sync_cleanup_guard_for_test());
    assert!(
        try_registered_provider(&registry, "oauprov").is_none(),
        "with no live parent the last sibling's teardown must remove the id, \
         not restore the finished sibling"
    );

    drop(sibling_b_runtime);
    drop(sibling_b);
    drop(sibling_a_runtime);
}

/// SNAP7 P2-3: parallel siblings A (first) and B (second); A ends before B.
/// B's restore chain still contains A, but A's stamp is gone, so B must skip
/// it and restore the parent provider.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn sibling_cleanup_restores_parent_after_earlier_sibling_finished() {
    let temp = TempDir::new("catalog-sibling-cleanup-parent");
    let agent_dir = TempDir::new("catalog-sibling-cleanup-parent-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let registry = test_registry();

    let parent_runtime = start_oauth_runtime(&temp, "parent").await;
    let parent = oauth_session(
        &registry,
        parent_runtime.clone(),
        temp.path(),
        &temp.path().join("parent-sessions"),
    );
    parent
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "parent");
    let parent_provider = Arc::downgrade(&registered_provider(&registry, "oauprov"));

    let sibling_a_runtime = start_oauth_runtime(&temp, "sibling-a").await;
    let sibling_a = oauth_session(
        &registry,
        sibling_a_runtime.clone(),
        temp.path(),
        &temp.path().join("sibling-a-sessions"),
    );
    sibling_a
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "sibling-a");

    let sibling_b_runtime = start_oauth_runtime(&temp, "sibling-b").await;
    let sibling_b = oauth_session(
        &registry,
        sibling_b_runtime.clone(),
        temp.path(),
        &temp.path().join("sibling-b-sessions"),
    );
    sibling_b
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "sibling-b");

    drop(sibling_a.provider_sync_cleanup_guard_for_test());
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "sibling-b",
        "the earlier sibling's cleanup must leave the current writer in place"
    );

    drop(sibling_b.provider_sync_cleanup_guard_for_test());
    let expected_parent = parent_provider
        .upgrade()
        .expect("the sibling restore chain must keep the parent provider alive");
    assert!(
        Arc::ptr_eq(&registered_provider(&registry, "oauprov"), &expected_parent),
        "the last sibling must skip the finished sibling's allocation and \
         restore the still-live parent"
    );
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "parent",
        "the restored OAuth overlay must dispatch to the parent runtime"
    );

    drop(sibling_b_runtime);
    drop(sibling_b);
    drop(sibling_a_runtime);
    drop(sibling_a);
    drop(parent_runtime);
    drop(parent);
}

/// SNAP7 P2-3 LIFO order: B ends first while A still runs, so B restores A's
/// exact allocation; A then restores the parent.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn sibling_cleanup_lifo_restores_live_sibling_then_parent() {
    let temp = TempDir::new("catalog-sibling-cleanup-lifo");
    let agent_dir = TempDir::new("catalog-sibling-cleanup-lifo-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let registry = test_registry();

    let parent_runtime = start_oauth_runtime(&temp, "parent").await;
    let parent = oauth_session(
        &registry,
        parent_runtime.clone(),
        temp.path(),
        &temp.path().join("parent-sessions"),
    );
    parent
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    let parent_provider = Arc::downgrade(&registered_provider(&registry, "oauprov"));

    let sibling_a_runtime = start_oauth_runtime(&temp, "sibling-a").await;
    let sibling_a = oauth_session(
        &registry,
        sibling_a_runtime.clone(),
        temp.path(),
        &temp.path().join("sibling-a-sessions"),
    );
    sibling_a
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    let sibling_a_provider = registered_provider(&registry, "oauprov");

    let sibling_b_runtime = start_oauth_runtime(&temp, "sibling-b").await;
    let sibling_b = oauth_session(
        &registry,
        sibling_b_runtime.clone(),
        temp.path(),
        &temp.path().join("sibling-b-sessions"),
    );
    sibling_b
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "sibling-b");

    // B ends first: A is still live and owns the exact allocation, so the
    // restore chain returns to A.
    drop(sibling_b.provider_sync_cleanup_guard_for_test());
    assert!(
        Arc::ptr_eq(
            &registered_provider(&registry, "oauprov"),
            &sibling_a_provider
        ),
        "LIFO teardown must restore the still-live sibling's allocation"
    );
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "sibling-a");

    // A ends next: its own chain restores the parent.
    drop(sibling_a.provider_sync_cleanup_guard_for_test());
    let expected_parent = parent_provider
        .upgrade()
        .expect("the parent provider must stay alive across both siblings");
    assert!(Arc::ptr_eq(
        &registered_provider(&registry, "oauprov"),
        &expected_parent
    ));
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "parent");

    drop(sibling_b_runtime);
    drop(sibling_b);
    drop(sibling_a_runtime);
    drop(sibling_a);
    drop(parent_runtime);
    drop(parent);
}

/// SNAP6 P2-2: a child whose provider definition disappears between two
/// syncs no longer reinstalls that provider, but its teardown ownership
/// record must still contain the old allocation. Otherwise the child's
/// dynamic entry survives in the shared registry and the parent keeps
/// resolving the child's dead wrapper.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn child_session_cleanup_after_definition_removal_still_removes_owned_provider() {
    let temp = TempDir::new("catalog-child-cleanup-defs-removed");
    let agent_dir = TempDir::new("catalog-child-cleanup-defs-removed-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let cwd = temp.path().to_string_lossy().into_owned();
    let registry = test_registry();

    let (plugin, enabled) = ToggleProviderPlugin::new();
    let mut config = agent_config_with_registry(
        temp.path(),
        SessionManager::create(&cwd, temp.path()).expect("session"),
        Arc::clone(&registry),
    );
    config.session = None;
    let child = AgentSession::new_with_plugins(config, vec![Arc::new(plugin)]);

    child.sync_plugin_providers_for_test().await;
    let installed = registered_provider(&registry, "toggle");
    assert!(!is_js_stream_provider(installed.as_ref()));

    // The definition disappears (a live/partial pass shape): the full pass
    // installs nothing for `toggle`, and the previous pass's ownership record
    // must be carried forward instead of being replaced by an empty stamp.
    *enabled
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
    child
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    assert!(
        try_registered_provider(&registry, "toggle").is_some(),
        "the sync does not unregister removed definitions by itself"
    );

    drop(child.provider_sync_cleanup_guard_for_test());
    assert!(
        try_registered_provider(&registry, "toggle").is_none(),
        "child teardown must still reclaim a provider whose definition was \
         removed after its last full sync"
    );
}

/// SNAP3 P2-2: a provider definition switching plain -> `streamSimple` ->
/// plain must restore the plain dynamic provider. A live pass deliberately
/// writes no sync stamp; keeping the earlier plain stamp let the reversion
/// short-circuit on it and leave the `JsStreamProvider` installed (its
/// always-authenticated api-key auth keeps serving requests). Clearing the
/// session stamp on every live pass makes the final plain sync rebuild.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn stream_simple_rollback_to_plain_reinstalls_the_plain_provider() {
    let temp = TempDir::new("catalog-stream-rollback");
    let agent_dir = TempDir::new("catalog-stream-rollback-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let cwd = temp.path().to_string_lossy().into_owned();
    let registry = test_registry();
    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");

    let mut config = agent_config_with_registry(
        temp.path(),
        SessionManager::create(&cwd, temp.path()).expect("session"),
        Arc::clone(&registry),
    );
    config.extensions = Some(handle);
    let plugin = Arc::new(MutableStreamProviderPlugin::new());
    let agent = AgentSession::new_with_plugins(config, vec![plugin.clone()]);

    // Initial plain definition: a normal dynamic provider, and a stamp is
    // written.
    agent.sync_plugin_providers_for_test().await;
    let plain = registered_provider(&registry, "delta");
    assert!(
        !is_js_stream_provider(plain.as_ref()),
        "the initial definition must install a plain dynamic provider"
    );

    // Flip the definition to streamSimple through a registry rebuild (the
    // `/reload` hot-reload shape). The live pass must install the JS provider.
    plugin.set_stream_simple(true);
    agent
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    let stream = registered_provider(&registry, "delta");
    assert!(
        is_js_stream_provider(stream.as_ref()),
        "streamSimple must install the JS stream provider"
    );
    assert!(
        !Arc::ptr_eq(&plain, &stream),
        "the live pass must replace the provider allocation"
    );

    // Flip back to the byte-identical plain definition. Without invalidating
    // the old stamp the sync short-circuits and `stream` stays installed.
    plugin.set_stream_simple(false);
    agent
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    let plain_again = registered_provider(&registry, "delta");
    assert!(
        !is_js_stream_provider(plain_again.as_ref()),
        "plain -> streamSimple -> plain must restore the plain provider, not \
         keep the JsStreamProvider (SNAP2 kept the old stamp)"
    );
    assert!(
        !Arc::ptr_eq(&stream, &plain_again),
        "the rollback must rebuild the provider instead of reusing the JS one"
    );
}

/// SNAP8 P2-1: provider resolution has a session dimension.
///
/// The parent and a REAL `spawn_child_runtime` child share one registry and
/// register the same OAuth provider id (`oauprov`). While the child is alive
/// the parent's `/login` and OAuth credential refresh must resolve the
/// parent's provider (its OAuth flow executes the parent's JS callback), the
/// child must resolve its own, and a parallel sibling must resolve its own.
/// The child's overlay disappears at teardown; the parent keeps working.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn session_provider_overlay_isolates_parent_child_and_sibling_oauth_flows() {
    let temp = TempDir::new("catalog-provider-overlay-oauth");
    let agent_dir = TempDir::new("catalog-provider-overlay-oauth-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let auth_path = temp.path().join("auth.json");
    let mut base =
        ProviderRegistry::default().with_auth_storage(Arc::new(FileAuthStorage::new(auth_path)));
    base.set_provider(Arc::new(CatalogProvider::new("alpha", 3)));
    base.set_provider(Arc::new(CatalogProvider::new("beta", 2)));
    let registry = Arc::new(base);

    let parent_runtime = start_oauth_runtime(&temp, "parent").await;
    let parent = oauth_session(
        &registry,
        parent_runtime.clone(),
        temp.path(),
        &temp.path().join("parent-sessions"),
    );
    parent
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    let parent_key = parent.provider_session_key_for_test();
    let parent_provider = registered_provider(&registry, "oauprov");
    let oauth_model = parent_provider
        .get_models()
        .into_iter()
        .next()
        .expect("parent provider model");
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "parent");

    // Parent `/login` through the session-scoped entry point.
    let parent_login = registry
        .login_for_session(
            &parent_key,
            "oauprov",
            AuthType::OAuth,
            &NoopLoginInteraction,
            &LoginOptions::default(),
        )
        .await
        .expect("parent login");
    assert_eq!(
        parent_login.access_token.as_deref(),
        Some("parent"),
        "parent /login must execute the parent's OAuth callback"
    );
    // OAuth credential refresh on the request-auth path: the stored token is
    // expired, so the provider's refresh callback runs. `derived-*` names the
    // runtime that executed it.
    let parent_auth = registry
        .get_auth_for_session(&parent_key, &oauth_model)
        .await
        .expect("parent auth")
        .expect("parent provider configured");
    assert_eq!(
        parent_auth.auth.api_key.as_deref(),
        Some("derived-refreshed-parent"),
        "the parent's OAuth refresh must use the parent's flow"
    );
    // SNAP10 P2-S8-2: the TUI resolves its `/login` row flow through
    // `tui_oauth_flow_for` → `AgentSession::oauth_flow_for_provider` (session overlay first), and the
    // Radius `/share` token through `get_auth_for_provider`.
    let parent_flow = pir::tui::terminal::tui_oauth_flow_for(Some(&parent), "oauprov")
        .expect("parent session-scoped OAuth flow");
    assert_eq!(
        parent_flow
            .login(&NoopLoginInteraction, &LoginOptions::default())
            .await
            .expect("parent flow login")
            .access,
        "parent",
        "the TUI's session-scoped flow lookup must see the parent's overlay"
    );
    let parent_auth = parent
        .get_auth_for_provider("oauprov")
        .await
        .expect("parent session auth")
        .expect("parent provider configured");
    assert_eq!(
        parent_auth.auth.api_key.as_deref(),
        Some("derived-refreshed-parent"),
        "get_auth_for_provider must refresh through the parent's overlay"
    );

    // REAL child runtime (the `run_child` seam) sharing the parent registry.
    let child_path = write_oauth_extension(&temp, "child");
    parent_runtime.set_path_plan(vec![child_path]);
    let catalog = pir::agent_session::ModelCatalog::from_registry(&registry);
    let child_runtime =
        pir::plugins::subagent::spawn_child_runtime(&parent_runtime, temp.path(), &catalog)
            .await
            .expect("child runtime");
    let child = oauth_session(
        &registry,
        child_runtime.handle().clone(),
        temp.path(),
        &temp.path().join("child-sessions"),
    );
    child
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    let child_key = child.provider_session_key_for_test();
    assert_eq!(
        oauth_login_label(&registry, "oauprov").await,
        "child",
        "the child's sync owns the process-shared entry while it runs"
    );

    // Live-child window (the child has synced, the parent has NOT re-synced):
    // the process-shared map is the child's wrapper. These are the assertions
    // that detect a session-less lookup regression: the parent must keep and
    // execute its own allocation while the shared map points at the child.
    let parent_view_live = registry
        .provider_box_for_session_for_test(&parent_key, "oauprov")
        .expect("parent overlay provider while the child is live");
    assert!(
        Arc::ptr_eq(&parent_view_live, &parent_provider),
        "the parent must keep its own allocation while the child owns the shared map"
    );
    let parent_login = parent
        .login_provider(
            "oauprov",
            AuthType::OAuth,
            &NoopLoginInteraction,
            &LoginOptions::default(),
        )
        .await
        .expect("parent login with live child");
    assert_eq!(
        parent_login.access_token.as_deref(),
        Some("parent"),
        "parent /login in the live-child window must execute the parent's flow"
    );
    let parent_auth = registry
        .get_auth_for_session(&parent_key, &oauth_model)
        .await
        .expect("parent auth with live child")
        .expect("parent provider configured");
    assert_eq!(
        parent_auth.auth.api_key.as_deref(),
        Some("derived-refreshed-parent"),
        "the parent's OAuth refresh must not execute the live child's flow"
    );
    // The TUI's login-row flow lookup is session-scoped too: while the shared
    // map owns the child wrapper, the parent row still reports the parent's
    // flow and the child row its own.
    let parent_flow = pir::tui::terminal::tui_oauth_flow_for(Some(&parent), "oauprov")
        .expect("parent session-scoped OAuth flow with live child");
    assert_eq!(
        parent_flow
            .login(&NoopLoginInteraction, &LoginOptions::default())
            .await
            .expect("parent flow login with live child")
            .access,
        "parent",
        "the parent's TUI flow lookup must not see the child overlay"
    );
    let child_flow = pir::tui::terminal::tui_oauth_flow_for(Some(&child), "oauprov")
        .expect("child session-scoped OAuth flow");
    assert_eq!(
        child_flow
            .login(&NoopLoginInteraction, &LoginOptions::default())
            .await
            .expect("child flow login")
            .access,
        "child",
        "the child's TUI flow lookup must see its own overlay"
    );
    // Radius-style token auth through `get_auth_for_provider` must also stay
    // on the parent's overlay while the shared map points at the child.
    let parent_auth = parent
        .get_auth_for_provider("oauprov")
        .await
        .expect("parent session auth with live child")
        .expect("parent provider configured");
    assert_eq!(
        parent_auth.auth.api_key.as_deref(),
        Some("derived-refreshed-parent"),
        "get_auth_for_provider must use the parent's overlay, not the child's"
    );

    // Child `/login` while the shared map still points at the child.
    let child_login = registry
        .login_for_session(
            &child_key,
            "oauprov",
            AuthType::OAuth,
            &NoopLoginInteraction,
            &LoginOptions::default(),
        )
        .await
        .expect("child login");
    assert_eq!(
        child_login.access_token.as_deref(),
        Some("child"),
        "the child must execute its own OAuth flow"
    );

    // The parent's next prompt re-syncs while the child is alive. The shared
    // map flips back to the parent, but the full pass rebuilds the wrapper
    // (the child currently owns the map, so the fast path cannot match), so
    // the overlay must follow the allocation THIS re-sync produced. Comparing
    // it with the pre-child `parent_provider` is the SNAP8 P2-S8-1 bug: it is
    // a different allocation by construction.
    parent.sync_plugin_providers_for_test().await;
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "parent");
    let parent_view = registry
        .provider_box_for_session_for_test(&parent_key, "oauprov")
        .expect("parent overlay provider after re-sync");
    assert_eq!(
        oauth_provider_label(&parent_view).await,
        "parent",
        "after the re-sync the parent must resolve its current wrapper, not the child's"
    );
    let child_view = registry
        .provider_box_for_session_for_test(&child_key, "oauprov")
        .expect("child overlay provider");
    assert_eq!(
        oauth_provider_label(&child_view).await,
        "child",
        "the child must resolve its own overlay entry, not the parent's"
    );
    assert!(
        !Arc::ptr_eq(&parent_view, &child_view),
        "parent and child overlays must stay distinct allocations"
    );

    // Parent `/login` after the re-sync still resolves the parent. The shared
    // map is the parent here, so this is a compatibility check; the
    // live-child window above is the regression-detecting one.
    let parent_login = registry
        .login_for_session(
            &parent_key,
            "oauprov",
            AuthType::OAuth,
            &NoopLoginInteraction,
            &LoginOptions::default(),
        )
        .await
        .expect("parent login after re-sync");
    assert_eq!(parent_login.access_token.as_deref(), Some("parent"));

    // Child `/login` after the parent's re-sync: still the child, even though
    // the shared map now points at the parent.
    let child_login = registry
        .login_for_session(
            &child_key,
            "oauprov",
            AuthType::OAuth,
            &NoopLoginInteraction,
            &LoginOptions::default(),
        )
        .await
        .expect("child login after parent re-sync");
    assert_eq!(
        child_login.access_token.as_deref(),
        Some("child"),
        "the child must not resolve the parent's re-synced shared entry"
    );

    // Parallel sibling with the same provider id: isolated from both.
    let sibling_runtime = start_oauth_runtime(&temp, "sibling").await;
    let sibling = oauth_session(
        &registry,
        sibling_runtime.clone(),
        temp.path(),
        &temp.path().join("sibling-sessions"),
    );
    sibling
        .refresh_plugin_registry_and_sync_providers_for_test()
        .await;
    let sibling_key = sibling.provider_session_key_for_test();
    let sibling_view = registry
        .provider_box_for_session_for_test(&sibling_key, "oauprov")
        .expect("sibling overlay provider");
    assert_eq!(
        oauth_provider_label(&sibling_view).await,
        "sibling",
        "the sibling must resolve its own runtime"
    );
    let sibling_login = registry
        .login_for_session(
            &sibling_key,
            "oauprov",
            AuthType::OAuth,
            &NoopLoginInteraction,
            &LoginOptions::default(),
        )
        .await
        .expect("sibling login");
    assert_eq!(sibling_login.access_token.as_deref(), Some("sibling"));
    assert_eq!(
        oauth_provider_label(
            &registry
                .provider_box_for_session_for_test(&parent_key, "oauprov")
                .expect("parent overlay entry")
        )
        .await,
        "parent"
    );
    assert_eq!(
        oauth_provider_label(
            &registry
                .provider_box_for_session_for_test(&child_key, "oauprov")
                .expect("child overlay entry")
        )
        .await,
        "child"
    );
    assert!(
        !Arc::ptr_eq(&sibling_view, &parent_view) && !Arc::ptr_eq(&sibling_view, &child_view),
        "the sibling allocation must stay distinct from the parent and child"
    );

    // Child teardown drops its overlay; the parent and sibling stay valid.
    drop(child.provider_sync_cleanup_guard_for_test());
    drop(child);
    drop(child_runtime);
    assert!(
        registry
            .provider_box_for_session_for_test(&child_key, "oauprov")
            .is_none(),
        "a finished child must not keep a resolvable provider overlay"
    );
    assert!(
        Arc::ptr_eq(
            &registry
                .provider_box_for_session_for_test(&parent_key, "oauprov")
                .expect("parent provider after child teardown"),
            &parent_view
        ),
        "the parent keeps its own allocation after child teardown"
    );
    let parent_login = registry
        .login_for_session(
            &parent_key,
            "oauprov",
            AuthType::OAuth,
            &NoopLoginInteraction,
            &LoginOptions::default(),
        )
        .await
        .expect("parent login after child teardown");
    assert_eq!(parent_login.access_token.as_deref(), Some("parent"));
    assert_eq!(oauth_login_label(&registry, "oauprov").await, "sibling");

    drop(sibling.provider_sync_cleanup_guard_for_test());
    drop(sibling_runtime);
    drop(sibling);
    drop(parent_runtime);
    drop(parent);
}

/// SNAP8 P2-1 plain regression: a single session's ordinary
/// `pi.registerProvider` definition is visible through both the session-scoped
/// and the legacy registry lookups, and its fast path still reuses the same
/// allocation.
#[tokio::test(flavor = "current_thread")]
async fn single_session_plain_provider_keeps_registry_visibility() {
    let temp = TempDir::new("catalog-provider-overlay-plain");
    let agent_dir = TempDir::new("catalog-provider-overlay-plain-agent");
    let _env = AgentDirOverride::new(agent_dir.path());
    let cwd = temp.path().to_string_lossy().into_owned();
    let registry = test_registry();
    let config = agent_config_with_registry(
        temp.path(),
        SessionManager::create(&cwd, temp.path()).expect("session"),
        Arc::clone(&registry),
    );
    let agent = AgentSession::new_with_plugins(config, vec![Arc::new(GammaProviderPlugin::new())]);
    agent.sync_plugin_providers_for_test().await;

    let key = agent.provider_session_key_for_test();
    let scoped = registry
        .provider_box_for_session_for_test(&key, "gamma")
        .expect("plain provider resolves through the session overlay");
    let global = registered_provider(&registry, "gamma");
    assert!(
        Arc::ptr_eq(&scoped, &global),
        "a single session's overlay and the process-shared map agree"
    );
    assert_eq!(
        agent.extension_api_key_for_test("gamma").as_deref(),
        Some("key-gamma"),
        "the plugin API key is still installed for the session"
    );

    // A second sync with the same definition reuses the allocation (the
    // pre-SNAP8 plain fast path is unchanged).
    agent.sync_plugin_providers_for_test().await;
    assert!(
        Arc::ptr_eq(&registered_provider(&registry, "gamma"), &global),
        "the unchanged plain definition must keep its provider allocation"
    );
}

/// B-line P1-1: a snapshot push handed the prompt run's already-built
/// projection must not build another full transcript projection.
#[tokio::test(flavor = "current_thread")]
async fn snapshot_push_with_prebuilt_projection_reuses_it() {
    let temp = TempDir::new("prebuilt-projection");
    let cwd = temp.path().to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
    manager
        .append_message(user_message("first user"))
        .expect("append user");
    manager
        .append_message(assistant_message("first reply"))
        .expect("append assistant");

    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
    let mut config = agent_config(temp.path(), manager);
    config.extensions = Some(handle.clone());
    let agent = AgentSession::new_with_plugins(config, Vec::new());

    let session = agent.session_manager().expect("persisted session");
    let projection = session
        .lock()
        .await
        .build_session_projection()
        .expect("projection");
    let before = session.lock().await.projection_build_count();

    assert!(
        agent
            .push_session_context_with_projection_for_test(&projection)
            .await,
        "the first push must build the snapshot"
    );
    let after = session.lock().await.projection_build_count();
    assert_eq!(
        after, before,
        "a snapshot push with a prebuilt projection must not rebuild it"
    );
    let snapshot = snapshot_json(&handle).await;
    assert_eq!(
        snapshot["entries"].as_array().map(Vec::len),
        Some(2),
        "the prebuilt projection still feeds the pushed entries: {snapshot}"
    );

    // A concurrent append makes the projection stale: the push must detect
    // the moved leaf and rebuild instead of publishing an old transcript.
    session
        .lock()
        .await
        .append_message(user_message("second user"))
        .expect("append second user");
    let stale_before = session.lock().await.projection_build_count();
    assert!(
        agent
            .push_session_context_with_projection_for_test(&projection)
            .await,
        "the moved leaf must invalidate the prebuilt projection"
    );
    let stale_after = session.lock().await.projection_build_count();
    assert!(
        stale_after > stale_before,
        "a stale prebuilt projection must fall back to live rebuild(s)"
    );
    let snapshot = snapshot_json(&handle).await;
    assert_eq!(
        snapshot["entries"].as_array().map(Vec::len),
        Some(3),
        "the rebuilt snapshot carries the appended entry: {snapshot}"
    );
}

/// R56 fragment-cache (PIR-SDK-027 round 2) regression block.
///
/// The per-entry projection views are cached as owned `JsonValue` fragments
/// plus the aggregate `Arc<JsonValue>`, the aggregate text and the FULL
/// `{entries, messages}` projection text. An append-only session EXTENDS the
/// cached set (one serialization per appended entry; every retained view
/// rides the cache), and any structural change (a label entry rewriting an
/// existing node's view, a fork/branch) falls back to a full fragment build.
/// The pushed wire snapshot must stay consistent with a full re-render in
/// BOTH paths -- the tests here assert the text and the counters.
mod fragment_cache_tests {
    use super::*;
    use pir::agent_session::{fragment_cache_stats_for_test, reset_fragment_cache_stats_for_test};

    async fn push_and_capture(handle: &ExtensionHandle, agent: &AgentSession) -> serde_json::Value {
        assert!(
            agent.push_session_context_for_test().await,
            "an appended entry must rebuild the snapshot"
        );
        snapshot_json(handle).await
    }

    /// Append-only session: every push after the first EXTENDS the cached
    /// fragment set (zero full builds after the first), and the pushed
    /// projection stays consistent with the live projection.
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
    async fn append_pushes_extend_the_cached_fragments_and_stay_byte_equal() {
        let _serial = env_lock();
        reset_fragment_cache_stats_for_test();
        let temp = TempDir::new("frag-extend");
        let cwd = temp.path().to_string_lossy().into_owned();
        let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
        manager
            .append_message(user_message("first user"))
            .expect("append user");
        manager
            .append_message(assistant_message("first reply"))
            .expect("append assistant");

        let handle =
            ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
        let mut config = agent_config(temp.path(), manager);
        config.extensions = Some(handle.clone());
        let agent = AgentSession::new_with_plugins(config, Vec::new());

        let first = push_and_capture(&handle, &agent).await;
        let (builds_first, _extends_first, _cloned_first) = fragment_cache_stats_for_test();
        assert_eq!(
            builds_first, 1,
            "the first push builds the fragment set for the full projection"
        );

        // Two appends: each push must EXTEND (no new full build) and the
        // wire text must carry the appended messages.
        let session = agent.session_manager().expect("persisted session");
        session
            .lock()
            .await
            .append_message(user_message("second user"))
            .expect("append second user");
        let second = push_and_capture(&handle, &agent).await;
        session
            .lock()
            .await
            .append_message(assistant_message("second reply"))
            .expect("append second reply");
        let third = push_and_capture(&handle, &agent).await;

        let (builds, extends, cloned) = fragment_cache_stats_for_test();
        assert_eq!(builds, 1, "appends must extend, never full-build");
        assert_eq!(extends, 2, "one extend per append push");
        assert_eq!(
            cloned, 0,
            "the cache hands the set over by move; no structural clone"
        );

        for snapshot in [&first, &second, &third] {
            let entries = snapshot["entries"].as_array().expect("entries is an array");
            let messages = snapshot["projection"]["messages"]
                .as_array()
                .expect("projection.messages is an array");
            assert_eq!(
                entries.len(),
                messages.len(),
                "every projected entry's view appears in the aggregate: {snapshot}"
            );
        }
        let third_texts: Vec<&str> = third["projection"]["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .filter_map(|message| {
                message["content"]
                    .as_array()
                    .and_then(|blocks| blocks.first())
                    .and_then(|block| block["text"].as_str())
            })
            .collect();
        assert_eq!(
            third_texts,
            vec!["first user", "first reply", "second user", "second reply"],
            "the spliced aggregate text carries the appended messages: {third}"
        );
        let third_views: Vec<&str> = third["projection"]["entries"]
            .as_array()
            .expect("entries array")
            .iter()
            .filter_map(|view| view["entry"]["id"].as_str())
            .collect();
        assert_eq!(
            third_views.len(),
            4,
            "the spliced views carry every projected entry: {third}"
        );
        assert_eq!(
            third["entries"].as_array().map(Vec::len),
            Some(4),
            "snapshot.entries is the flat message list: {third}"
        );
    }

    /// A label append moves the leaf to the label entry (a STATE entry
    /// projecting zero messages). The FRAGMENT set stays positionally valid —
    /// the label's empty view appends — so the push extends; the label is
    /// visible in the pushed label map and the entry tree, and a following
    /// plain append extends again. (The entry TREE does full-render on a
    /// label append — the label rewrites an existing node's rendered fields;
    /// covered by the tree cache unit test.)
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
    async fn label_append_extends_fragments_and_publishes_the_label() {
        let _serial = env_lock();
        reset_fragment_cache_stats_for_test();
        let temp = TempDir::new("frag-label");
        let cwd = temp.path().to_string_lossy().into_owned();
        let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
        manager
            .append_message(user_message("labeled turn"))
            .expect("append user");
        manager
            .append_message(assistant_message("labeled reply"))
            .expect("append assistant");

        let handle =
            ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
        let mut config = agent_config(temp.path(), manager);
        config.extensions = Some(handle.clone());
        let agent = AgentSession::new_with_plugins(config, Vec::new());

        let _first = push_and_capture(&handle, &agent).await;
        let (builds_first, _extends, _cloned) = fragment_cache_stats_for_test();
        assert_eq!(builds_first, 1, "the first push builds");

        let session = agent.session_manager().expect("persisted session");
        let leaf = session
            .lock()
            .await
            .leaf_id()
            .map(str::to_owned)
            .expect("leaf");
        session
            .lock()
            .await
            .set_label(&leaf, Some("checkpoint"))
            .expect("set label");
        let labeled = push_and_capture(&handle, &agent).await;
        let (builds, extends, _cloned) = fragment_cache_stats_for_test();
        assert_eq!(
            builds, builds_first,
            "a label append keeps the fragment set (zero-message view appended)"
        );
        assert_eq!(extends, 1, "the label push extended the cached set");
        assert_eq!(
            labeled["entries"].as_array().map(Vec::len),
            Some(2),
            "the label does not change the projected context: {labeled}"
        );
        assert_eq!(
            labeled["labels"]
                .as_object()
                .and_then(|labels| labels.get(&leaf))
                .and_then(|value| value.as_str()),
            Some("checkpoint"),
            "the pushed label map carries the new label: {labeled}"
        );

        // After the label push, a plain append extends again.
        reset_fragment_cache_stats_for_test();
        session
            .lock()
            .await
            .append_message(user_message("after label"))
            .expect("append after label");
        let after = push_and_capture(&handle, &agent).await;
        let (builds2, extends2, _cloned) = fragment_cache_stats_for_test();
        assert_eq!(builds2, 0, "the post-label append must extend");
        assert_eq!(extends2, 1, "exactly one extend");
        assert_eq!(
            after["projection"]["messages"].as_array().map(Vec::len),
            Some(3),
            "the appended message is in the aggregate: {after}"
        );
    }

    /// A BRANCH (fork to an earlier entry) reshuffles the retained-entry list
    /// the fragment vec is index-aligned with: the next push must FULL-BUILD
    /// the fragment set — extending would splice the wrong views at the wrong
    /// positions.
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
    async fn branch_falls_back_to_a_full_fragment_build() {
        let _serial = env_lock();
        reset_fragment_cache_stats_for_test();
        let temp = TempDir::new("frag-branch");
        let cwd = temp.path().to_string_lossy().into_owned();
        let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
        manager
            .append_message(user_message("root user"))
            .expect("append user");
        manager
            .append_message(assistant_message("root reply"))
            .expect("append assistant");

        let handle =
            ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
        let mut config = agent_config(temp.path(), manager);
        config.extensions = Some(handle.clone());
        let agent = AgentSession::new_with_plugins(config, Vec::new());

        let _first = push_and_capture(&handle, &agent).await;
        let (builds_first, _extends, _cloned) = fragment_cache_stats_for_test();
        assert_eq!(builds_first, 1, "the first push builds");

        // Branch to the FIRST message: the retained list becomes [root user]
        // — shorter than the cached two-entry set and reordered.
        let session = agent.session_manager().expect("persisted session");
        let root_user_id = session
            .lock()
            .await
            .resident_entries()
            .iter()
            .find(|entry| {
                !entry.is_header() && {
                    let id = entry.id();
                    id.is_some()
                }
            })
            .and_then(|entry| entry.id().map(str::to_owned))
            .expect("root user id");
        session
            .lock()
            .await
            .branch(&root_user_id)
            .expect("branch to root user");
        let branched = push_and_capture(&handle, &agent).await;
        let (builds, extends, _cloned) = fragment_cache_stats_for_test();
        assert!(
            builds > builds_first,
            "a branch must fall back to a full fragment build (list reshuffled)"
        );
        assert_eq!(extends, 0, "no extend may fire across a branch");
        assert_eq!(
            branched["projection"]["messages"].as_array().map(Vec::len),
            Some(1),
            "the branched projection carries only the branch point: {branched}"
        );
        assert_eq!(
            branched["leafId"].as_str(),
            Some(root_user_id.as_str()),
            "the pushed leaf is the branch point: {branched}"
        );

        // The full-build set is the new base: a plain append extends again.
        reset_fragment_cache_stats_for_test();
        session
            .lock()
            .await
            .append_message(user_message("after branch"))
            .expect("append after branch");
        let after = push_and_capture(&handle, &agent).await;
        let (builds2, extends2, _cloned) = fragment_cache_stats_for_test();
        assert_eq!(builds2, 0, "the post-branch append must extend");
        assert_eq!(extends2, 1, "exactly one extend");
        assert_eq!(
            after["projection"]["messages"].as_array().map(Vec::len),
            Some(2),
            "the appended message rides the spliced aggregate: {after}"
        );
    }
}

/// R7 branch-text cache regression block.
///
/// The snapshot's `branch` text is the leaf-path entry list serialized to one
/// JSON array. R7 caches that text across pushes and EXTENDS it by the
/// appended path entries instead of re-serializing the whole path every
/// prompt. These tests pin:
/// - the pushed branch text is byte-identical to a from-scratch render
///   (`serde_json::to_string(branch_entries(None))`) after appends, a label
///   append, a context edit and a compaction (the extend path, including a
///   splice of an already-spliced text), and after a `branch`, a
///   `branch_with_summary` re-parent and a storage replace (the full-render
///   fallback);
/// - the extend fires only for append-shaped changes and the fallback only
///   for structural ones (`branch_text_cache_stats_for_test`).
mod branch_text_cache_tests {
    use super::*;
    use pir::agent_session::session_ctx::{
        branch_text_cache_stats_for_test, reset_branch_text_cache_stats_for_test,
    };

    /// A from-scratch render of exactly the bytes the snapshot publishes: the
    /// live leaf path (root → leaf, the order `projected_branch_path`
    /// returns) serialized as a JSON array. No cache is involved.
    async fn branch_oracle(agent: &AgentSession) -> String {
        let session = agent.session_manager().expect("persisted session");
        let path = session
            .lock()
            .await
            .branch_entries(None)
            .expect("branch entries");
        serde_json::to_string(&path).expect("branch oracle")
    }

    fn recorded_branch(pushes: &Arc<Mutex<Vec<SessionContextSnapshot>>>) -> String {
        pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .expect("a recorded snapshot")
            .branch_json
            .clone()
    }

    /// One push that must rebuild, then: the recorded `branch_json` equals the
    /// from-scratch oracle byte for byte, and the JS wire form parses to the
    /// same value (the host splices the text raw).
    async fn push_and_check(
        agent: &AgentSession,
        pushes: &Arc<Mutex<Vec<SessionContextSnapshot>>>,
        handle: &ExtensionHandle,
        step: &str,
    ) -> String {
        assert!(
            agent.push_session_context_for_test().await,
            "{step}: the mutation must invalidate the snapshot cache"
        );
        let text = recorded_branch(pushes);
        let oracle = branch_oracle(agent).await;
        assert_eq!(
            text, oracle,
            "{step}: the pushed branch text must match a from-scratch render"
        );
        let wire: serde_json::Value = handle
            .eval_json("__pir_host.getSessionManagerJson()")
            .await
            .unwrap_or_else(|error| panic!("{step}: reading the pushed snapshot failed: {error}"));
        assert_eq!(
            wire["branch"],
            serde_json::from_str::<serde_json::Value>(&oracle).expect("oracle parses"),
            "{step}: the wire branch must parse to the from-scratch render"
        );
        text
    }

    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
    async fn branch_text_extends_on_appends_and_falls_back_on_structure_changes() {
        let _serial = env_lock();
        reset_branch_text_cache_stats_for_test();
        let temp = TempDir::new("branch-text");
        let cwd = temp.path().to_string_lossy().into_owned();
        let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
        manager
            .append_message(user_message("root user"))
            .expect("append user");
        manager
            .append_message(assistant_message("root reply"))
            .expect("append assistant");

        let (recorder, pushes) = SnapshotRecorder::new();
        let handle =
            ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
        let mut config = agent_config(temp.path(), manager);
        config.extensions = Some(handle.clone());
        let recorder: Arc<dyn AgentPlugin> = Arc::new(recorder);
        let agent = AgentSession::new_with_plugins(config, vec![recorder]);

        // (a) First push: full render.
        let first = push_and_check(&agent, &pushes, &handle, "first push").await;
        assert_eq!(
            branch_text_cache_stats_for_test(),
            (1, 0),
            "the first push full-renders the branch once"
        );

        let session = agent.session_manager().expect("persisted session");
        let root_user_id = session
            .lock()
            .await
            .resident_entries()
            .iter()
            .find(|entry| !entry.is_header() && entry.id().is_some())
            .and_then(|entry| entry.id().map(str::to_owned))
            .expect("root user id");

        // (b) Appends extend: the appended entries' text is spliced onto the
        // cached prefix.
        session
            .lock()
            .await
            .append_message(user_message("second user"))
            .expect("append user");
        session
            .lock()
            .await
            .append_message(assistant_message("second reply"))
            .expect("append assistant");
        let second = push_and_check(&agent, &pushes, &handle, "append").await;
        assert!(
            second.len() > first.len(),
            "the appended entries grew the branch text"
        );
        assert_eq!(
            branch_text_cache_stats_for_test(),
            (1, 1),
            "the append must extend, never full-render"
        );

        // (c) A label append moves the leaf to the label entry (it joins the
        // path): still an extension.
        let leaf = session
            .lock()
            .await
            .leaf_id()
            .map(str::to_owned)
            .expect("leaf");
        session
            .lock()
            .await
            .set_label(&leaf, Some("checkpoint"))
            .expect("set label");
        push_and_check(&agent, &pushes, &handle, "label append").await;
        assert_eq!(branch_text_cache_stats_for_test(), (1, 2));

        // (d) A context edit appends a branch-local entry: still an extension.
        session
            .lock()
            .await
            .append_context_edit(&root_user_id, Some(serde_json::json!("edited root")))
            .expect("context edit");
        push_and_check(&agent, &pushes, &handle, "context edit").await;
        assert_eq!(branch_text_cache_stats_for_test(), (1, 3));

        // (e) A compaction appends a checkpoint: still an extension.
        session
            .lock()
            .await
            .append_compaction("compacted summary", None, 1000, None)
            .expect("compaction");
        push_and_check(&agent, &pushes, &handle, "compaction").await;
        assert_eq!(branch_text_cache_stats_for_test(), (1, 4));

        // (f) `branch` to an earlier entry: the cached path is not a prefix of
        // the new one — full render.
        session
            .lock()
            .await
            .branch(&root_user_id)
            .expect("branch to root user");
        push_and_check(&agent, &pushes, &handle, "branch").await;
        assert_eq!(
            branch_text_cache_stats_for_test(),
            (2, 4),
            "a branch must full-render the branch text"
        );

        // (g) An append after the branch extends from the new base.
        session
            .lock()
            .await
            .append_message(user_message("after branch"))
            .expect("append after branch");
        push_and_check(&agent, &pushes, &handle, "append after branch").await;
        assert_eq!(branch_text_cache_stats_for_test(), (2, 5));

        // (h) `branch_with_summary` re-parents to an earlier entry: full render.
        session
            .lock()
            .await
            .branch_with_summary(Some(&root_user_id), "branch summary", None, None, None)
            .expect("branch with summary");
        push_and_check(&agent, &pushes, &handle, "branch with summary").await;
        assert_eq!(
            branch_text_cache_stats_for_test(),
            (3, 5),
            "a re-parent must full-render the branch text"
        );

        // (i) A storage replace (new session id + file): full render.
        let leaf = session
            .lock()
            .await
            .leaf_id()
            .map(str::to_owned)
            .expect("leaf");
        session
            .lock()
            .await
            .create_branched_session(&leaf)
            .expect("branch storage");
        push_and_check(&agent, &pushes, &handle, "storage replace").await;
        assert_eq!(
            branch_text_cache_stats_for_test(),
            (4, 5),
            "a storage switch must full-render the branch text"
        );

        // (j) `reset_leaf`: the empty path full-renders as `[]`, and the next
        // append extends from the EMPTY cached path (no leading comma).
        session.lock().await.reset_leaf();
        let empty = push_and_check(&agent, &pushes, &handle, "reset leaf").await;
        assert_eq!(
            empty, "[]",
            "a session with no leaf renders an empty branch"
        );
        assert_eq!(branch_text_cache_stats_for_test(), (5, 5));
        session
            .lock()
            .await
            .append_message(user_message("after reset"))
            .expect("append after reset");
        let after_reset = push_and_check(&agent, &pushes, &handle, "append after reset").await;
        assert_eq!(
            branch_text_cache_stats_for_test(),
            (5, 6),
            "the post-reset append must extend the empty cached path"
        );
        assert!(
            after_reset.starts_with("[{"),
            "the empty-path splice must not emit a leading comma: {after_reset}"
        );
    }

    /// FALSIFICATION PIN (lane x5, adversarial review of 89ce717; issue
    /// `.play/issues/PIR-BIN-105.md`): a same-path storage reload is NOT a
    /// cache-key change. `set_session_file` on the SAME file after an
    /// external rewrite that keeps every entry id (same id, same position,
    /// new bytes) leaves `session_id`/`session_path` equal and the covered
    /// path ids a prefix of the live path, so the next append-shaped push
    /// SPLICES the stale cached bytes for the already-covered entry. Pre-R7
    /// the branch was re-rendered from the live path and served the new
    /// bytes. FIXED (PIR-BIN-105): the branch-text key now carries the manager's
    /// storage generation, so a same-path reload (bumped by build_index)
    /// refuses the splice and full-renders. This pin stays green.
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
    async fn branch_text_same_path_reload_serves_stale_bytes() {
        let _serial = env_lock();
        let temp = TempDir::new("branch-reload");
        let cwd = temp.path().to_string_lossy().into_owned();
        let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
        manager
            .append_message(user_message("original user text"))
            .expect("append user");
        manager
            .append_message(assistant_message("reply"))
            .expect("append assistant");
        let session_file = manager
            .session_file()
            .expect("persisted session path")
            .to_path_buf();

        let (recorder, pushes) = SnapshotRecorder::new();
        let handle =
            ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
        let mut config = agent_config(temp.path(), manager);
        config.extensions = Some(handle.clone());
        let recorder: Arc<dyn AgentPlugin> = Arc::new(recorder);
        let agent = AgentSession::new_with_plugins(config, vec![recorder]);

        // (a) First push caches the branch text for [user, assistant].
        assert!(
            agent.push_session_context_for_test().await,
            "first push builds"
        );
        assert_eq!(recorded_branch(&pushes), branch_oracle(&agent).await);

        // (b) External same-file rewrite: keep the header and every entry id,
        // change the user message's text (the ordinary hand-edit shape).
        let raw = std::fs::read_to_string(&session_file).expect("read session file");
        let mut lines: Vec<String> = raw.lines().map(str::to_owned).collect();
        let mut patched = 0usize;
        for line in lines.iter_mut() {
            let mut value: serde_json::Value =
                serde_json::from_str(line).expect("session line parses");
            if value.get("type").and_then(|kind| kind.as_str()) != Some("message") {
                continue;
            }
            if value
                .pointer("/message/role")
                .and_then(|role| role.as_str())
                != Some("user")
            {
                continue;
            }
            let content = value
                .pointer_mut("/message/content")
                .and_then(|content| content.as_array_mut())
                .expect("user content is an array");
            content[0]["text"] = serde_json::json!("MUTATED EXTERNAL TEXT");
            *line = serde_json::to_string(&value).expect("re-encode line");
            patched += 1;
        }
        assert_eq!(patched, 1, "exactly the user message line was rewritten");
        std::fs::write(&session_file, format!("{}\n", lines.join("\n")))
            .expect("rewrite session file");

        // (c) Reload the SAME path in place: same session id, same path, same
        // entry ids/count/leaf. No push runs between reload and append — a
        // fingerprint HIT push would drop the cached branch (it is `take`n on
        // every push entry) and mask the splice.
        let session = agent.session_manager().expect("persisted session");
        session
            .lock()
            .await
            .set_session_file(&session_file, None)
            .expect("same-path reload");

        // (d) One append-shaped push: the covered ids still prefix-match, so
        // the cached (pre-rewrite) bytes for the user message are reused.
        session
            .lock()
            .await
            .append_message(user_message("after reload"))
            .expect("append after reload");
        assert!(
            agent.push_session_context_for_test().await,
            "the append invalidates the snapshot cache"
        );

        let text = recorded_branch(&pushes);
        let oracle = branch_oracle(&agent).await;
        assert_eq!(
            text, oracle,
            "a same-path reload must not serve stale covered-entry bytes"
        );
        let wire: serde_json::Value = handle
            .eval_json("__pir_host.getSessionManagerJson()")
            .await
            .expect("the pushed snapshot parses");
        assert_eq!(
            wire["branch"],
            serde_json::from_str::<serde_json::Value>(&oracle).expect("oracle parses"),
            "the wire branch must match the from-scratch render"
        );
    }

    /// FALSIFICATION PIN (lane x5, adversarial review of 89ce717; issue
    /// `.play/issues/PIR-BIN-106.md`): the R7 empty-projection guard only
    /// catches cached sets with ZERO entries (`cached_views == 0`). A cached
    /// set whose entries ALL project zero messages (e.g. a
    /// `thinking_level_change`-only path) carries the SAME empty rendered
    /// aggregate text (`aggregate_text == "[]"`, `projection_json`
    /// `...],"messages":[]}`), takes the `cached_views > 0` extend path,
    /// and the extender's text splice strips only the closing bracket —
    /// emitting a leading comma (`"messages":[,{...}]`) into the pushed
    /// envelope. `getSessionManagerJson` splices that text raw, so the wire
    /// reply is unparsable. Expected to FAIL until the caller/extender also
    /// refuses to splice from an EMPTY rendered aggregate.
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
    async fn zero_message_cached_projection_splice_emits_invalid_json() {
        let _serial = env_lock();
        let temp = TempDir::new("empty-aggregate");
        let cwd = temp.path().to_string_lossy().into_owned();
        let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
        manager
            .append_thinking_level_change("high")
            .expect("append thinking level change");

        let (recorder, pushes) = SnapshotRecorder::new();
        let handle =
            ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
        let mut config = agent_config(temp.path(), manager);
        config.extensions = Some(handle.clone());
        let recorder: Arc<dyn AgentPlugin> = Arc::new(recorder);
        let agent = AgentSession::new_with_plugins(config, vec![recorder]);
        let session = agent.session_manager().expect("persisted session");

        let last_projection = |pushes: &Arc<Mutex<Vec<SessionContextSnapshot>>>| {
            pushes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .last()
                .expect("a recorded snapshot")
                .projection_json
                .clone()
        };

        // (a) First push: the projection holds ONE entry that projects ZERO
        // messages, and the cached fragment set still carries rendered text.
        assert!(
            agent.push_session_context_for_test().await,
            "first push builds"
        );
        let first = last_projection(&pushes);
        assert!(
            first.ends_with("\"messages\":[]}"),
            "the first push's aggregate is the empty array: {first}"
        );

        // (b) One append that DOES project a message: `cached_views > 0`, so
        // the R7 guard does not apply and the aggregate text is spliced from
        // the cached EMPTY text.
        session
            .lock()
            .await
            .append_message(user_message("first message"))
            .expect("append user");
        assert!(
            agent.push_session_context_for_test().await,
            "the append invalidates the snapshot cache"
        );
        let text = last_projection(&pushes);
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&text);
        assert!(
            parsed.is_ok(),
            "the pushed projection text must stay valid JSON: {text}"
        );
        // The same invalid text is spliced RAW into the JS wire envelope.
        let wire = handle.eval_json("__pir_host.getSessionManagerJson()").await;
        assert!(
            wire.is_ok(),
            "the pushed ctx.sessionManager envelope must parse: {wire:?}"
        );
    }
}

/// A host with NO JS face (builtin/inline plugins only, `config.extensions =
/// None`) has no reader for the JS-only session views: the push must not
/// render or retain the branch text, the tree text/arena, the label map or
/// the projection text, while the native contract (`entries` + ids/path/cwd/
/// header/leaf/model) stays exactly what the JS-face push publishes.
///
/// The `entries` oracle is the live projection's flat message list serialized
/// with `serde_json::to_value` — the same value the JS path splices, so a
/// native push that changed it would fail here.
mod native_snapshot_shape_tests {
    use super::*;

    async fn native_push(pushes: &Arc<Mutex<Vec<SessionContextSnapshot>>>, agent: &AgentSession) {
        assert!(
            agent.push_session_context_for_test().await,
            "a fresh session must rebuild the native snapshot"
        );
        assert_eq!(
            pushes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "one push reaches the native fan-out"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
    async fn native_only_push_skips_the_js_only_views_but_keeps_entries() {
        let _serial = env_lock();
        let temp = TempDir::new("native-js-gate");
        let cwd = temp.path().to_string_lossy().into_owned();
        let mut manager = SessionManager::create(&cwd, temp.path()).expect("session");
        manager
            .append_message(user_message("gate question"))
            .expect("append user");
        manager
            .append_message(assistant_message("gate reply"))
            .expect("append assistant");

        let (recorder, pushes) = SnapshotRecorder::new();
        let config = agent_config(temp.path(), manager);
        let agent = AgentSession::new_with_plugins(config, vec![Arc::new(recorder)]);
        native_push(&pushes, &agent).await;

        let snapshot = pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .expect("recorded snapshot");
        assert!(
            snapshot.branch_json.is_empty(),
            "no JS face: the branch text must not be rendered: {:?}",
            snapshot.branch_json
        );
        assert!(
            snapshot.tree_json.is_empty(),
            "no JS face: the tree text must not be rendered"
        );
        assert!(
            snapshot.projection_json.is_empty(),
            "no JS face: the projection text must not be rendered"
        );
        assert_eq!(
            snapshot.labels,
            serde_json::Value::Object(serde_json::Map::new()),
            "no JS face: the label map is not walked"
        );
        assert!(snapshot.session_id.is_some(), "native ids stay populated");
        assert!(snapshot.cwd.is_some(), "native cwd stays populated");
        assert!(snapshot.leaf_id.is_some(), "native leaf stays populated");
        assert!(
            !snapshot.header.is_null(),
            "native header stays populated: {}",
            snapshot.header
        );

        // `entries` is the oracle: the live projection's flat message list.
        let oracle = {
            let session = agent.session_manager().expect("persisted session");
            let projection = session
                .lock()
                .await
                .build_session_projection()
                .expect("projection");
            serde_json::to_value(&projection.messages).expect("oracle json")
        };
        assert_eq!(
            snapshot.entries.as_ref(),
            &oracle,
            "the native entries payload is byte-identical to the projection messages"
        );

        // The extend path (an append) keeps the same shape: one extend, no
        // full rebuild, entries grown by the appended turn, no texts.
        let session = agent.session_manager().expect("persisted session");
        session
            .lock()
            .await
            .append_message(user_message("after push"))
            .expect("append after push");
        assert!(
            agent.push_session_context_for_test().await,
            "the appended entry must rebuild"
        );
        let grown = pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .expect("second snapshot");
        assert!(
            grown.branch_json.is_empty()
                && grown.tree_json.is_empty()
                && grown.projection_json.is_empty(),
            "the extend path keeps the JS-only views unrendered"
        );
        assert_eq!(
            grown.entries.as_array().map(Vec::len),
            Some(3),
            "the appended user turn is in the native aggregate: {}",
            grown.entries
        );
    }
}
