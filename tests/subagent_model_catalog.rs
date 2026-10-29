//! Model-catalog ordering for fresh extension runtimes.
//!
//! `spawn_child_runtime` loads the parent's extension path plan into a fresh
//! actor BEFORE any `AgentSession` exists. Extension factories evaluate
//! synchronously during that load and the installed `pi-subagents`
//! model-resolver shim resolves `modelRuntime.getModels()` there; with an
//! empty catalog it fails the child with
//! "No models available. Check your installation or add models to
//! models.json.". These tests pin the fix: the parent's serialized catalog is
//! pushed onto the fresh handle before `load`, so:
//!
//! 1. a load-time factory sees the configured model/provider with no session
//!    context push at all (the `#[doc(hidden)]` spawn seam);
//! 2. dynamically registered providers are included (the registry's
//!    `get_models` merges them);
//! 3. a builtin-only child handle stays plan-only (no QuickJS, catalog push is
//!    a harmless shared-state write);
//! 4. the native `subagent` tool's child runtime sees a non-empty catalog at
//!    factory time (integration, hermetic temp file);
//! 5. the shared loader's catalog-aware variant pushes pre-load too.
//!
//! The model is faked with a scripted provider (same pattern as
//! `tests/subagent_builtin.rs`); the extension records the catalog it saw so
//! no assumption about runtime state order is needed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pir::agent::tool_execution::{AgentTool, ToolContext, ToolRegistry, ToolSessionEnv};
use pir::agent_session::ModelCatalog;
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::providers::{DynamicProviderConfig, RegisterDynamicProviderShared};
use pir::ai::types::*;
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::plugins::{instantiate_builtin, SubagentPlugin};

// ---------------------------------------------------------------------------
// Scripted provider
// ---------------------------------------------------------------------------

fn model(provider: &str, id: &str) -> Model {
    Model {
        id: id.into(),
        name: format!("{provider} {id}"),
        api: "fake-api".into(),
        provider: provider.into(),
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

fn fake_model() -> Model {
    model("fake", "fake-model")
}

fn assistant(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        usage: Usage {
            input: 10,
            output: 5,
            total_tokens: 15,
            cost: Cost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: 0.25,
                ..Cost::default()
            },
            ..Usage::default()
        },
        timestamp: 1,
        ..Default::default()
    }
}

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
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            assistant("CHILD_OK"),
        ));
        stream
    }
}

fn fake_registry() -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
    }));
    Arc::new(registry)
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-subagent-catalog-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn write_agent(&self, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        let dir = self.0.join("agents");
        std::fs::create_dir_all(&dir).expect("create agents dir");
        let path = dir.join(name);
        std::fs::write(&path, format!("---\n{frontmatter}---\n{body}")).expect("write agent");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write an extension whose factory records the catalog it resolved at load
/// time (`globalThis.__pirFactoryCatalog`) and, when `record` is set, writes
/// the parsed catalog JSON to that file. `record` is embedded as a JSON
/// string literal so Windows paths round-trip through the fs shim.
fn write_catalog_extension(ext_path: &Path, record: Option<&Path>) {
    let record_line = match record {
        Some(record) => format!(
            "  try {{\n    fs.writeFileSync({}, JSON.stringify(catalog));\n  }} catch (error) {{\n    globalThis.__pirFactoryWriteError = String((error && error.message) || error);\n  }}",
            serde_json::to_string(&record.display().to_string()).expect("encode record path")
        ),
        None => String::new(),
    };
    let source = format!(
        r#"
import fs from "node:fs";

export default function (pi) {{
  // Runs synchronously during the runtime's load command: exactly where the
  // pi-subagents model-resolver shim resolves its models.
  const catalog = JSON.parse(__pir_host.getModelsJson() || "{{}}");
  globalThis.__pirFactoryCatalog = catalog;
{record_line}
}}
"#
    );
    std::fs::write(ext_path, source).expect("write extension");
}

/// A plan-only parent (no actor of its own) carrying `ext_path` as its
/// recorded plan: the child spawns the only runtime that loads it.
fn plan_only_parent(cwd: &Path, ext_path: &Path) -> ExtensionHandle {
    let parent = ExtensionHandle::start_plan_only(ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        ..Default::default()
    });
    parent.set_path_plan(vec![ext_path.to_path_buf()]);
    parent
}

fn has_model(catalog: &serde_json::Value, provider: &str, id: &str) -> bool {
    catalog
        .get("models")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|models| {
            models.iter().any(|model| {
                model.get("provider").and_then(serde_json::Value::as_str) == Some(provider)
                    && model.get("id").and_then(serde_json::Value::as_str) == Some(id)
            })
        })
}

fn has_provider(catalog: &serde_json::Value, id: &str) -> bool {
    catalog
        .get("providers")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|providers| {
            providers
                .iter()
                .any(|provider| provider.get("id").and_then(serde_json::Value::as_str) == Some(id))
        })
}

async fn factory_catalog(handle: &ExtensionHandle) -> serde_json::Value {
    handle
        .eval_json("JSON.stringify(globalThis.__pirFactoryCatalog ?? null)")
        .await
        .expect("factory catalog probe")
}

async fn live_catalog(handle: &ExtensionHandle) -> serde_json::Value {
    handle
        // getModelsJson already returns a JSON string; eval_json parses it,
        // so do not JSON.stringify it a second time (that yields a string
        // value holding the JSON text).
        .eval_json("__pir_host.getModelsJson()")
        .await
        .expect("live catalog probe")
}

// ---------------------------------------------------------------------------
// 1. Pre-load push: factory sees the catalog with no session context push
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn child_runtime_sees_model_catalog_before_any_session_push() {
    let temp = TempDir::new("preload");
    let ext_path = temp.0.join("ext.ts");
    write_catalog_extension(&ext_path, None);
    let parent = plan_only_parent(&temp.0, &ext_path);
    let registry = fake_registry();
    let catalog = ModelCatalog::from_registry(&registry);

    // No `AgentSession` is built here, so nothing has pushed a session
    // context: the only way the factory can see a model is the pre-load push
    // inside `spawn_child_runtime`.
    let child = pir::plugins::subagent::spawn_child_runtime(&parent, &temp.0, &catalog)
        .await
        .expect("child runtime spawns");
    let seen = factory_catalog(child.handle()).await;
    assert!(
        has_model(&seen, "fake", "fake-model"),
        "factory did not see the configured model: {seen}"
    );
    assert!(
        has_provider(&seen, "fake"),
        "factory did not see the configured provider: {seen}"
    );

    let live = live_catalog(child.handle()).await;
    assert!(
        has_model(&live, "fake", "fake-model"),
        "getModelsJson after load is missing the model: {live}"
    );
    assert!(
        has_provider(&live, "fake"),
        "getModelsJson after load is missing the provider: {live}"
    );
}

// ---------------------------------------------------------------------------
// 2. Dynamically registered providers are part of the child catalog
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn dynamic_provider_models_are_included_in_the_child_catalog() {
    let temp = TempDir::new("dynamic");
    let ext_path = temp.0.join("ext.ts");
    write_catalog_extension(&ext_path, None);
    let parent = plan_only_parent(&temp.0, &ext_path);

    let registry = fake_registry();
    registry
        .register_dynamic_provider_shared(
            "dynamic-host",
            DynamicProviderConfig {
                base_url: "http://localhost".to_string(),
                api: "openai-chat".to_string(),
                models: vec![Model {
                    // Empty api/base_url are stamped from the provider config
                    // (`openai-chat`), so construction takes the same adapter
                    // path a real extension-registered provider uses.
                    api: String::new(),
                    base_url: String::new(),
                    ..model("dynamic-host", "dynamic-model")
                }],
                ..Default::default()
            },
        )
        .expect("register dynamic provider");
    let catalog = ModelCatalog::from_registry(&registry);
    let child = pir::plugins::subagent::spawn_child_runtime(&parent, &temp.0, &catalog)
        .await
        .expect("child runtime spawns");

    let seen = factory_catalog(child.handle()).await;
    assert!(
        has_model(&seen, "fake", "fake-model"),
        "static model missing from the child catalog: {seen}"
    );
    assert!(
        has_model(&seen, "dynamic-host", "dynamic-model"),
        "dynamic provider model missing from the child catalog: {seen}"
    );
    assert!(
        has_provider(&seen, "dynamic-host"),
        "dynamic provider id missing from the child catalog: {seen}"
    );
    let live = live_catalog(child.handle()).await;
    assert!(
        has_model(&live, "dynamic-host", "dynamic-model"),
        "getModelsJson after load is missing the dynamic model: {live}"
    );
}

// ---------------------------------------------------------------------------
// 3. A builtin-only plan stays plan-only (catalog push is a shared-state write)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn builtin_only_child_handle_stays_plan_only() {
    let temp = TempDir::new("builtin-only");
    let parent = ExtensionHandle::start_plan_only(ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        ..Default::default()
    });
    parent.set_path_plan(vec![PathBuf::from("builtin:subagent")]);
    let registry = fake_registry();
    let catalog = ModelCatalog::from_registry(&registry);

    let child = pir::plugins::subagent::spawn_child_runtime(&parent, &temp.0, &catalog)
        .await
        .expect("plan-only child handle");
    assert!(
        !child.handle().has_js_runtime(),
        "a builtin-only child plan must not construct the QuickJS runtime"
    );
    assert!(
        child
            .handle()
            // getModelsJson already returns a JSON string; eval_json parses it,
            // so do not JSON.stringify it a second time (that yields a string
            // value holding the JSON text).
            .eval_json("__pir_host.getModelsJson()")
            .await
            .is_err(),
        "a plan-only handle must have no JS engine to evaluate against"
    );
}

// ---------------------------------------------------------------------------
// 4. Integration: the native subagent tool's child factory sees the catalog
// ---------------------------------------------------------------------------

fn session_env(
    registry: Arc<ProviderRegistry>,
    user_agents_dir: Option<PathBuf>,
) -> Arc<ToolSessionEnv> {
    Arc::new(ToolSessionEnv {
        provider_registry: registry,
        model: fake_model(),
        thinking: ThinkingLevel::Off,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(ToolRegistry::new()),
        has_ui: false,
        project_trusted: true,
        user_agents_dir,
        agent_dir: None,
        credential: None,
        on_update: None,
        live_tools: None,
        active_override: None,
        background: None,
    })
}

fn tool_context(cwd: &Path, env: Arc<ToolSessionEnv>) -> ToolContext {
    ToolContext {
        cwd: cwd.to_path_buf(),
        signal: None,
        nested: None,
        session_env: Some(env),
        held_mutation_keys: None,
        on_update: None,
        background: false,
    }
}

async fn start_parent_runtime(cwd: &Path, ext_path: &Path) -> ExtensionHandle {
    let handle = ExtensionHandle::start(ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("fake-model".to_string()),
        session_id: Some("catalog-test-parent".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime start");
    handle.set_path_plan(vec![ext_path.to_path_buf()]);
    let load = handle
        .load(&[ext_path.to_path_buf()])
        .await
        .expect("load extension");
    assert!(load.errors.is_empty(), "{:?}", load.errors);
    handle
}

fn subagent_tool_with_handle(handle: ExtensionHandle) -> Arc<dyn AgentTool> {
    let plugin = instantiate_builtin("subagent").expect("instantiates");
    let subagent_plugin = plugin
        .as_any()
        .and_then(|any| any.downcast_ref::<SubagentPlugin>())
        .expect("subagent plugin downcast");
    subagent_plugin.attach_js(Some(handle));
    let mut registrations = pir::plugins::PluginRegistrations::default();
    let mut registration = pir::plugins::RegistrationContext::new(&mut registrations);
    plugin.register(&mut registration).expect("register");
    let backend = registrations
        .tools
        .iter()
        .find(|(def, _)| def.name == "subagent")
        .map(|(_, backend)| backend.clone())
        .expect("registered subagent tool");
    let pir::plugins::plugin::ToolBackend::Rust(tool) = backend else {
        panic!("subagent must register a Rust tool backend");
    };
    tool
}

#[tokio::test(flavor = "current_thread")]
async fn subagent_child_factory_sees_a_non_empty_catalog() {
    let temp = TempDir::new("integration");
    temp.write_agent("scout.md", "name: scout\ndescription: d\n", "Answer.");
    let record = temp.0.join("child-catalog.json");
    let ext_path = temp.0.join("ext.ts");
    write_catalog_extension(&ext_path, Some(&record));

    // Parent load runs the same factory first (recording the empty parent
    // catalog); the child's fresh runtime must overwrite it with its own
    // non-empty snapshot.
    let parent = start_parent_runtime(&temp.0, &ext_path).await;
    let registry = fake_registry();
    let tool = subagent_tool_with_handle(parent);
    let env = session_env(registry, Some(temp.0.join("agents")));
    let ctx = tool_context(&temp.0, env);

    let result = tool
        .run(
            &serde_json::json!({ "agent": "scout", "task": "record" }),
            &ctx,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("CHILD_OK"),
        "child run produced no output: {}",
        result.content
    );

    let recorded = std::fs::read_to_string(&record).expect("child factory recorded the catalog");
    let catalog: serde_json::Value =
        serde_json::from_str(&recorded).expect("recorded catalog is JSON");
    assert!(
        has_model(&catalog, "fake", "fake-model"),
        "child factory saw an empty/wrong catalog: {catalog}"
    );
    assert!(
        has_provider(&catalog, "fake"),
        "child factory did not see the provider: {catalog}"
    );
}

// ---------------------------------------------------------------------------
// 5. The shared loader pushes the catalog pre-load too
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn loader_pushes_catalog_before_loading_extensions() {
    let temp = TempDir::new("loader");
    let ext_path = temp.0.join("ext.ts");
    write_catalog_extension(&ext_path, None);
    let registry = fake_registry();
    let catalog = ModelCatalog::from_registry(&registry);

    let handle = pir::agent_session::load_extension_runtime_with_catalog(
        std::slice::from_ref(&ext_path),
        ExtensionConfig {
            cwd: temp.0.clone(),
            mode: "headless".to_string(),
            has_ui: false,
            ..Default::default()
        },
        Some(&catalog),
    )
    .await
    .expect("runtime loads");

    let seen = factory_catalog(&handle).await;
    assert!(
        has_model(&seen, "fake", "fake-model"),
        "loader did not push the catalog before load: {seen}"
    );
    assert!(
        has_provider(&seen, "fake"),
        "loader did not push providers before load: {seen}"
    );
}
