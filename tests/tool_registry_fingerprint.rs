//! P0-4: the merged tool registry is keyed on DEFINITIONS, not just names.
//!
//! `AgentSession::merged_tool_registry` used to cache on the sorted tool-name
//! snapshot plus the active override. MCP marks the registry dirty when a tool
//! keeps its name but changes its description, input schema, output schema or
//! exposure, and the host rebuilds — but the name-keyed cache kept returning
//! the old `AgentTool` objects, so validation, tool-repair and the model
//! guidance saw stale schemas.
//!
//! The cache now keys on [`PluginHost::registry_generation`], which moves on
//! every successful registry rebuild. A hit is one `u64` compare plus the
//! override read; no tool definitions are cloned.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{Model, ModelCost, ModelCostRates, ThinkingLevel};
use pir::plugins::plugin::{AgentPlugin, RegistrationContext};
use pir::plugins::PluginInfo;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-tool-registry-fingerprint-{label}-{}-{}",
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
        context_window: 8192,
        max_tokens: 512,
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

/// A tool whose definition depends on the plugin's revision, recreating the
/// `AgentTool` object on every registration pass. The NAME never changes, so
/// only a definition-aware cache key can notice the update.
struct RevisionedTool {
    revision: u64,
}

impl AgentTool for RevisionedTool {
    fn name(&self) -> &'static str {
        "mcp__s__t"
    }
    fn description(&self) -> &'static str {
        if self.revision == 0 {
            "old description"
        } else {
            "new description"
        }
    }
    fn parameters(&self) -> Option<Value> {
        if self.revision == 0 {
            Some(json!({
                "type": "object",
                "properties": {
                    "old_argument": { "type": "string" }
                }
            }))
        } else {
            Some(json!({
                "type": "object",
                "properties": {
                    "fresh_argument": { "type": "boolean" }
                },
                "required": ["fresh_argument"]
            }))
        }
    }
    fn output_schema(&self) -> Option<Value> {
        if self.revision == 0 {
            Some(json!({
                "type": "object",
                "properties": {
                    "old_output": { "type": "string" }
                }
            }))
        } else {
            Some(json!({
                "type": "object",
                "properties": {
                    "fresh_output": { "type": "number" }
                }
            }))
        }
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

/// Registers [`RevisionedTool`] and lets a test move the revision between
/// registration passes (the fake MCP definition change).
struct RevisionedPlugin {
    revision: AtomicU64,
}

impl RevisionedPlugin {
    fn new() -> Self {
        Self {
            revision: AtomicU64::new(0),
        }
    }

    fn revise(&self) {
        self.revision.store(1, Ordering::SeqCst);
    }
}

impl AgentPlugin for RevisionedPlugin {
    fn info(&self) -> &PluginInfo {
        static INFO: std::sync::OnceLock<PluginInfo> = std::sync::OnceLock::new();
        INFO.get_or_init(|| PluginInfo::inline("tool-registry-fingerprint-test"))
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(RevisionedTool {
            revision: self.revision.load(Ordering::SeqCst),
        }));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The definition-aware cache
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn definition_changes_invalidate_a_kept_name_and_cache_hits_do_not_rebuild() {
    let dir = TempDir::new("kept-name");
    let plugin = Arc::new(RevisionedPlugin::new());
    let session = AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: Arc::new(ProviderRegistry::default()),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 1,
            system_prompt: Some("base system prompt".into()),
            cwd: Some(dir.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    let host = session.plugin_host().expect("inline plugin host");
    let generation = host.registry_generation();

    // First materialization: the old definition.
    let (first, builds_first) = session.merged_tool_registry_for_test(host);
    let old = first.get("mcp__s__t").expect("registered tool");
    assert_eq!(old.description(), "old description");
    assert_eq!(
        old.parameters(),
        Some(json!({
            "type": "object",
            "properties": { "old_argument": { "type": "string" } }
        }))
    );
    assert_eq!(
        old.output_schema(),
        Some(json!({
            "type": "object",
            "properties": { "old_output": { "type": "string" } }
        }))
    );

    // Unchanged second call: cache hit (same registry, same counter).
    let (second, builds_second) = session.merged_tool_registry_for_test(host);
    assert_eq!(
        builds_second, builds_first,
        "an unchanged call must not materialize the registry again"
    );
    assert!(
        Arc::ptr_eq(&first, &second),
        "an unchanged call returns the cached registry"
    );
    assert_eq!(
        host.registry_generation(),
        generation,
        "an unchanged call does not rebuild the host registry"
    );

    // Change the definition while keeping the name, then rebuild the host
    // registry (the path MCP's dirty flag and JS re-registrations take).
    plugin.revise();
    host.rebuild_registry().await;
    assert_eq!(
        host.registry_generation(),
        generation + 1,
        "every successful rebuild moves the generation"
    );
    let rebuilt_def = host
        .registry()
        .resolved_tools()
        .into_iter()
        .find(|(def, _)| def.name == "mcp__s__t")
        .map(|(def, _)| def.clone())
        .expect("the host publishes the rebuilt definition");
    assert_eq!(rebuilt_def.description, "new description");

    // Next call: generation changed, so the stale tool object is replaced.
    let (third, builds_third) = session.merged_tool_registry_for_test(host);
    assert_eq!(
        builds_third,
        builds_first + 1,
        "a moved generation misses the cache once"
    );
    assert!(
        !Arc::ptr_eq(&second, &third),
        "the kept-name definition change replaces the stale registry"
    );
    let fresh = third.get("mcp__s__t").expect("registered tool");
    assert_eq!(fresh.description(), "new description");
    assert_eq!(
        fresh.parameters(),
        Some(json!({
            "type": "object",
            "properties": { "fresh_argument": { "type": "boolean" } },
            "required": ["fresh_argument"]
        }))
    );
    assert_eq!(
        fresh.output_schema(),
        Some(json!({
            "type": "object",
            "properties": { "fresh_output": { "type": "number" } }
        }))
    );

    // And the rebuilt entry is cached again.
    let (fourth, builds_fourth) = session.merged_tool_registry_for_test(host);
    assert_eq!(builds_fourth, builds_third);
    assert!(Arc::ptr_eq(&third, &fourth));

    session.shutdown().await;
}
