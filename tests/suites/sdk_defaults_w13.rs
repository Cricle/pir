//! W13-G SDK defaults / plugin-registration-ledger tests.
//!
//! Two contracts pinned here:
//!
//! * **(A) Model resolution precedence.** `AgentBuilder` must apply the
//!   settings `defaultProvider`/`defaultModel` when the caller named no model,
//!   and an explicit model choice must win. The precedence itself is
//!   `ai::providers::resolution`'s; these tests exercise it hermetically (the
//!   resolver takes the model pool as an argument, so no ambient
//!   `~/.pi/agent` state can change the outcome), plus one single-build
//!   end-to-end case through the builder.
//!
//! * **(B) Plugin registration ledger.** A plugin whose `register()` fails is
//!   isolated from dispatch but must be REPORTED: the host's ledger names it,
//!   a registry rebuild does not lose the record, and the SDK build refuses to
//!   hand back an agent silently missing a plugin the caller attached.
//!
//! The end-to-end settings tests redirect the process-global `PIR_AGENT_DIR`
//! through `tests/common/mod.rs::AgentDirEnvGuard`, which holds the ONE
//! binary-wide agent-dir env lock shared with every other agent-dir suite in
//! the merged `g_agent` binary (a module-local lock cannot exclude them).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::json;

use pir::agent::tool_batch::ToolResult;
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::providers::construction::model;
use pir::ai::providers::resolution::{
    find_initial_model, resolve_cli_model, InitialModelOptions, ScopedModel,
};
use pir::ai::types::{
    AssistantMessage, Context, Model, StopReason, StreamOptions, TerminalReason, ThinkingLevel,
    Usage,
};
use pir::plugins::{AgentPlugin, PluginHost, PluginInfo, RegistrationContext};
use pir::sdk::{AgentBuilder, SdkError};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-sdk-defaults-w13-{label}-{}-{}",
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

/// Pin `PIR_AGENT_DIR` at `dir` with `PI_OFFLINE=1` for the test's duration,
/// on the binary-wide lock (`tests/common/mod.rs::AgentDirEnvGuard`).
///
/// Offline keeps `install_remote_catalog_overlays` from spawning refresh tasks
/// for a catalog this fixture does not need. The guard restores both variables
/// on drop (panic included), so a sibling suite never observes the pin.
async fn pin_agent_dir(dir: &Path) -> crate::common::AgentDirEnvGuard {
    crate::common::AgentDirEnvGuard::at(dir, &[("PI_OFFLINE", "1")]).await
}

fn test_model(provider: &str, id: &str) -> Model {
    model(
        provider,
        id,
        id,
        "openai-completions",
        "http://127.0.0.1:9/v1",
        128_000,
        8_192,
        vec!["text"],
    )
}

/// The pool shape `models.json` produces for the fixture below: one custom
/// provider with two models, plus an unrelated provider.
fn fixture_pool() -> Vec<Model> {
    vec![
        test_model("w13d", "w13d-default"),
        test_model("w13d", "w13d-other"),
        test_model("other", "other-model"),
    ]
}

// ---------------------------------------------------------------------------
// (A) Resolution precedence — hermetic (the pool is an argument)
// ---------------------------------------------------------------------------

#[test]
fn explicit_provider_qualified_model_resolves_when_the_pool_has_it() {
    let pool = fixture_pool();

    // The exact shape `AgentBuilder::model_in_provider("w13d", "w13d-other")`
    // and the CLI's `--provider w13d --model w13d-other` use.
    let (resolved, thinking, warning) =
        resolve_cli_model(Some("w13d"), "w13d-other", &pool).expect("qualified id resolves");
    let resolved = resolved.expect("model");
    assert_eq!(
        (resolved.provider.as_str(), resolved.id.as_str()),
        ("w13d", "w13d-other")
    );
    assert!(thinking.is_none());
    assert!(
        warning.is_none(),
        "a known id must not fall back to a custom model id: {warning:?}"
    );

    // A bare id works too (`AgentBuilder::model("w13d-other")`).
    let (resolved, _, _) = resolve_cli_model(None, "w13d-other", &pool).expect("bare id resolves");
    assert_eq!(resolved.expect("model").id, "w13d-other");
}

#[test]
fn saved_settings_defaults_apply_only_without_an_explicit_model() {
    let pool = fixture_pool();
    let always_auth = |_: &str| true;
    let empty_scope: Vec<ScopedModel> = Vec::new();
    let empty_levels: BTreeMap<String, ThinkingLevel> = BTreeMap::new();

    // Saved default (settings `defaultProvider`/`defaultModel`).
    let result = find_initial_model(InitialModelOptions {
        default_provider: Some("w13d"),
        default_model_id: Some("w13d-default"),
        scoped_models: &empty_scope,
        model_thinking_levels: &empty_levels,
        models: &pool,
        has_auth: &always_auth,
        ..InitialModelOptions::default()
    })
    .expect("resolution");
    let saved = result.model.expect("saved default");
    assert_eq!(
        (saved.provider.as_str(), saved.id.as_str()),
        ("w13d", "w13d-default"),
        "the saved settings default selects the model when the caller names none"
    );
}

#[test]
fn explicit_model_pattern_outranks_the_saved_defaults() {
    let pool = fixture_pool();
    let always_auth = |_: &str| true;
    let empty_scope: Vec<ScopedModel> = Vec::new();
    let empty_levels: BTreeMap<String, ThinkingLevel> = BTreeMap::new();

    // Upstream `findInitialModel` order: the explicit reference is resolved
    // FIRST, so the saved default never overrides it.
    let result = find_initial_model(InitialModelOptions {
        cli_model: Some("w13d-other"),
        default_provider: Some("w13d"),
        default_model_id: Some("w13d-default"),
        scoped_models: &empty_scope,
        model_thinking_levels: &empty_levels,
        models: &pool,
        has_auth: &always_auth,
        ..InitialModelOptions::default()
    })
    .expect("resolution");
    let explicit = result.model.expect("explicit model");
    assert_eq!(
        (explicit.provider.as_str(), explicit.id.as_str()),
        ("w13d", "w13d-other"),
        "an explicit model choice must win over the saved settings default"
    );
}

// ---------------------------------------------------------------------------
// (B) Plugin registration ledger
// ---------------------------------------------------------------------------

/// A plugin whose `register()` fails until `healthy` is flipped, so a rebuild
/// can be observed both ways.
struct FlakyPlugin {
    info: PluginInfo,
    healthy: Arc<AtomicBool>,
    registered: Arc<AtomicBool>,
}

impl AgentPlugin for FlakyPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, _ctx: &mut RegistrationContext<'_>) -> Result<(), anyhow::Error> {
        if !self.healthy.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("intentional register failure"));
        }
        self.registered.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn registration_failure_is_recorded_in_the_host_ledger() {
    let healthy = Arc::new(AtomicBool::new(false));
    let registered = Arc::new(AtomicBool::new(false));
    let host = PluginHost::build(
        None,
        vec![Arc::new(FlakyPlugin {
            info: PluginInfo::inline("w13-flaky"),
            healthy: Arc::clone(&healthy),
            registered,
        })],
    );

    let failures = host.registration_failures();
    assert_eq!(
        failures.len(),
        1,
        "a failed register() must be recorded in the ledger, got {failures:?}"
    );
    assert_eq!(failures[0].path, "<inline:w13-flaky>");
    assert!(
        failures[0].message.contains("intentional register failure"),
        "the ledger carries the plugin's own error: {:?}",
        failures[0].message
    );
    assert!(
        !host.all_plugins_registered(),
        "a ledgered failure must not read as success"
    );
}

/// The ledger must survive a registry rebuild.
///
/// This is the regression the `std::mem::take` bug produced at the rebuild
/// call site: the diagnostics vector was drained into the registry BEFORE the
/// ledger was derived from it, so every rebuild recomputed an empty ledger and
/// a standing failure silently disappeared.
#[tokio::test(flavor = "current_thread")]
async fn registration_failure_survives_a_registry_rebuild() {
    let host = PluginHost::build(
        None,
        vec![Arc::new(FlakyPlugin {
            info: PluginInfo::inline("w13-flaky"),
            healthy: Arc::new(AtomicBool::new(false)),
            registered: Arc::new(AtomicBool::new(false)),
        })],
    );
    assert_eq!(host.registration_failures().len(), 1);

    host.rebuild_registry().await;
    let failures = host.registration_failures();
    assert_eq!(
        failures.len(),
        1,
        "a rebuild must not invent a clean ledger: {failures:?}"
    );
    assert_eq!(failures[0].path, "<inline:w13-flaky>");
}

/// Minimal provider for the hermetic `with_registry` build below; never
/// streams in this test.
struct IdleProvider;

impl Provider for IdleProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn name(&self) -> &str {
        "Fake provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![test_model("fake", "fake-model")]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let message = AssistantMessage {
            api: "openai-completions".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            stop_reason: StopReason::Stop,
            usage: Usage::default(),
            ..Default::default()
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_build_surfaces_an_attached_plugin_registration_failure() {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(IdleProvider));

    let result = AgentBuilder::new()
        .with_registry(
            Arc::new(registry),
            model(
                "fake",
                "fake-model",
                "fake-model",
                "openai-completions",
                "http://127.0.0.1:9/v1",
                4_096,
                256,
                vec!["text"],
            ),
        )
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .with_plugin(Arc::new(FlakyPlugin {
            info: PluginInfo::inline("w13-failing"),
            healthy: Arc::new(AtomicBool::new(false)),
            registered: Arc::new(AtomicBool::new(false)),
        }))
        .build()
        .await;

    match result {
        Ok(_) => panic!("a failed plugin registration must fail the SDK build"),
        Err(SdkError::Other(message)) => {
            assert!(
                message.contains("<inline:w13-failing>"),
                "the build error names the plugin: {message}"
            );
            assert!(
                message.contains("intentional register failure"),
                "the build error carries the register error: {message}"
            );
        }
        Err(other) => panic!("expected SdkError::Other, got {other:?}"),
    }

    // The isolation half of the contract: a sibling plugin still attaches and
    // registers while one plugin fails (the failure is reported, not fatal to
    // the rest of the surface).
    let healthy = Arc::new(AtomicBool::new(true));
    let registered = Arc::new(AtomicBool::new(false));
    let host = PluginHost::build(
        None,
        vec![
            Arc::new(FlakyPlugin {
                info: PluginInfo::inline("w13-failing"),
                healthy: Arc::new(AtomicBool::new(false)),
                registered: Arc::new(AtomicBool::new(false)),
            }),
            Arc::new(FlakyPlugin {
                info: PluginInfo::inline("w13-survivor"),
                healthy: Arc::clone(&healthy),
                registered: Arc::clone(&registered),
            }),
        ],
    );
    assert_eq!(host.registration_failures().len(), 1);
    assert!(
        registered.load(Ordering::SeqCst),
        "the surviving plugin must still run its register() pass"
    );
    assert_eq!(
        host.plugins()
            .iter()
            .map(|plugin| plugin.info().path.as_str())
            .collect::<Vec<_>>(),
        vec!["<inline:w13-survivor>"],
        "the failed plugin is isolated out of the dispatch list"
    );
}

// ---------------------------------------------------------------------------
// (A) End-to-end: `AgentBuilder` applies the settings model defaults
// ---------------------------------------------------------------------------

/// Write a models.json with one authenticated provider and two models, plus a
/// settings.json naming the saved default.
fn write_settings_fixture(agent_dir: &Path) {
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::to_string_pretty(&json!({
            "providers": {
                "w13d": {
                    "api": "openai-completions",
                    "baseUrl": "http://127.0.0.1:9/v1",
                    "apiKey": "w13d-test-key",
                    "models": [
                        { "id": "w13d-default" },
                        { "id": "w13d-other" }
                    ]
                }
            }
        }))
        .expect("serialize models.json"),
    )
    .expect("write models.json");
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "defaultProvider": "w13d",
            "defaultModel": "w13d-default"
        }))
        .expect("serialize settings.json"),
    )
    .expect("write settings.json");
}

#[tokio::test(flavor = "current_thread")]
async fn builder_applies_the_settings_model_defaults() {
    let agent_dir = TempDir::new("agent-defaults");
    let cwd = TempDir::new("cwd-defaults");
    write_settings_fixture(&agent_dir.0);
    let _env = pin_agent_dir(&agent_dir.0).await;

    // The guard holds the binary-wide agent-dir lock for the whole test, so no
    // sibling suite can flip `PIR_AGENT_DIR` under this build.
    let agent = AgentBuilder::new()
        .no_session()
        .no_compaction()
        .cwd(cwd.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .tool(
            "probe",
            "probe tool",
            json!({ "type": "object" }),
            |_args| async { ToolResult::ok("probe") },
        )
        .build()
        .await
        .expect("build with the settings model defaults");

    assert_eq!(agent.model().provider, "w13d");
    assert_eq!(agent.model().id, "w13d-default");
}
