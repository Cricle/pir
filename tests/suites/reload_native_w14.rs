//! W14-E regression tests for the two env-audit defects (`docs/archive/env-audit-w14.md`).
//!
//! D3: the phone builtin's Codex-quota route must resolve `<agent-dir>/auth.json`
//! through the ONE canonical agent-dir resolver. Its old private resolver read
//! only `PI_CODING_AGENT_DIR`, so a harness pinning the documented primary
//! variable (`PIR_AGENT_DIR`) made the route read the operator's real
//! `~/.pi/agent/auth.json`.
//!
//! D4: `/reload` must re-run the native plugin lifecycle. `PluginHost::reload`
//! had no product caller; the TUI `/reload` reached the plugin state through
//! [`AgentSession::reload_plugins`] → `PluginHost::reload_native_plugins`, and
//! the JS runtime half keeps running through the extension dispatcher exactly
//! once.
//!
//! NOTE: registered by the foreman (`#[path = "suites/reload_native_w14.rs"]`
//! in a `g_*` merged binary). It uses `crate::common::AgentDirEnvGuard`, so
//! register it in a binary that includes `mod common;` (`tests/g_agent.rs`
//! or `tests/g_tools.rs`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{Model, ModelCost, ModelCostRates, ThinkingLevel};
use pir::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginInfo,
    RegistrationContext,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-w14e-{label}-{}-{}",
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

/// Unset one variable for the duration of a test and restore it on drop
/// (panic included). Only used for the codemode heap override, which would
/// otherwise mask the settings value the cache-invalidation test asserts.
#[cfg(feature = "extensions")]
struct UnsetEnvVar(&'static str, Option<std::ffi::OsString>);

#[cfg(feature = "extensions")]
impl UnsetEnvVar {
    fn new(name: &'static str) -> Self {
        let saved = std::env::var_os(name);
        std::env::remove_var(name);
        Self(name, saved)
    }
}

#[cfg(feature = "extensions")]
impl Drop for UnsetEnvVar {
    fn drop(&mut self) {
        match self.1.take() {
            Some(value) => std::env::set_var(self.0, value),
            None => std::env::remove_var(self.0),
        }
    }
}

fn fake_model() -> Model {
    Model {
        id: "w14e-model".into(),
        name: "W14-E model".into(),
        api: "fake-api".into(),
        provider: "w14e-fake".into(),
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

fn session_config(
    cwd: &Path,
    extensions: Option<pir::extensions::ExtensionHandle>,
) -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: Some("W14-E reload".to_owned()),
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions,
        tool_selection: None,
    }
}

/// A native plugin that records the lifecycle the reload seam must drive:
/// `register()` (one per registry build), `on_reload()`, and the native
/// `session_shutdown {reason: "reload"}` teardown event.
struct ReloadRecorder {
    info: PluginInfo,
    registrations: Arc<AtomicUsize>,
    reloads: Arc<AtomicUsize>,
    shutdown_reloads: Arc<AtomicUsize>,
    fail_reload: bool,
}

impl ReloadRecorder {
    fn new(name: &str, fail_reload: bool) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            registrations: Arc::new(AtomicUsize::new(0)),
            reloads: Arc::new(AtomicUsize::new(0)),
            shutdown_reloads: Arc::new(AtomicUsize::new(0)),
            fail_reload,
        })
    }

    fn reloads(&self) -> usize {
        self.reloads.load(Ordering::SeqCst)
    }

    fn registrations(&self) -> usize {
        self.registrations.load(Ordering::SeqCst)
    }

    fn shutdown_reloads(&self) -> usize {
        self.shutdown_reloads.load(Ordering::SeqCst)
    }
}

impl AgentPlugin for ReloadRecorder {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["session_shutdown".to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            if event.name == "session_shutdown"
                && event.payload.get("reason").and_then(|value| value.as_str()) == Some("reload")
            {
                self.shutdown_reloads.fetch_add(1, Ordering::SeqCst);
            }
            PluginEventOutcome::None
        })
    }

    fn register(&self, _ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        self.registrations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn on_reload(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.reloads.fetch_add(1, Ordering::SeqCst);
            if self.fail_reload {
                anyhow::bail!("synthetic on_reload failure");
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// D3 — the phone quota credential path follows the canonical resolver
// ---------------------------------------------------------------------------

#[cfg(feature = "phone")]
#[tokio::test(flavor = "current_thread")]
async fn phone_quota_auth_path_honours_pir_agent_dir() {
    let temp = TempDir::new("phone");
    let decoy = temp.0.join("decoy-pi-coding-agent-dir");
    std::fs::create_dir_all(&decoy).expect("create decoy dir");
    let decoy_text = decoy.to_string_lossy().into_owned();
    // PIR_AGENT_DIR is pinned by the guard; PI_CODING_AGENT_DIR is the decoy the
    // OLD private resolver would have used. The canonical resolver must pick
    // the primary variable.
    let _guard = crate::common::AgentDirEnvGuard::at(
        &temp.0,
        &[("PI_CODING_AGENT_DIR", decoy_text.as_str())],
    )
    .await;

    let resolved = pir::plugins::builtin::phone::server::quota_auth_path();

    assert_eq!(
        resolved,
        temp.0.join("auth.json"),
        "D3: the quota route must read the PIR_AGENT_DIR credential file"
    );
    assert_ne!(
        resolved,
        decoy.join("auth.json"),
        "D3: PIR_AGENT_DIR must win over PI_CODING_AGENT_DIR"
    );
    // The operator's real `~/.pi/agent` is never consulted while the primary
    // variable is set (the empty pinned dir cannot contain a credential).
    if let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
    {
        assert_ne!(
            resolved,
            home.join(".pi").join("agent").join("auth.json"),
            "D3: a pinned agent dir must never fall back to the real home dir"
        );
    }
}

// ---------------------------------------------------------------------------
// D4 (b) — the session /reload seam runs the native lifecycle exactly once
// and invalidates the process-wide codemode config cache
// ---------------------------------------------------------------------------

#[cfg(feature = "extensions")]
#[tokio::test(flavor = "current_thread")]
async fn session_reload_runs_native_hooks_and_invalidates_the_codemode_cache() {
    let temp = TempDir::new("reload-codemode");
    let settings_path = temp.0.join("settings.json");
    std::fs::write(&settings_path, r#"{"codemode":{"sandboxMemoryMb":96}}"#)
        .expect("write pinned settings");
    let _guard = crate::common::AgentDirEnvGuard::at(&temp.0, &[]).await;
    let _override = UnsetEnvVar::new("PIR_CODEMODE_MEMORY_MB");

    let recorder = ReloadRecorder::new("w14e-reload-recorder", false);
    let session = AgentSession::new_with_plugins(
        session_config(&temp.0, None),
        vec![
            Arc::clone(&recorder) as Arc<dyn AgentPlugin>,
            pir::plugins::codemode::new_codemode_plugin(),
        ],
    );

    // Seed the process-wide extension-route cache from the first settings
    // snapshot (the state `/reload` must invalidate). `reload_cached_...` is
    // the unconditional reset, so a value left by another test in this merged
    // binary cannot mask the assertion below.
    let primed = pir::plugins::codemode::reload_cached_codemode_config();
    assert_eq!(primed.sandbox_memory_bytes, Some(96 * 1024 * 1024));

    std::fs::write(&settings_path, r#"{"codemode":{"sandboxMemoryMb":32}}"#)
        .expect("rewrite pinned settings");
    let loads_before = pir::plugins::codemode::settings_load_count();

    session
        .reload_plugins()
        .await
        .expect("the session reload seam runs");

    assert_eq!(
        recorder.reloads(),
        1,
        "D4: one session /reload runs the plugin-host reload pass exactly once"
    );
    assert_eq!(
        recorder.shutdown_reloads(),
        1,
        "D4: native plugins observed session_shutdown {{reason: \"reload\"}} once"
    );
    assert!(
        recorder.registrations() >= 2,
        "D4: the reload rebuilt the registry (register() ran again), got {}",
        recorder.registrations()
    );
    // The counter is process-wide and another suite in the same binary may
    // construct a codemode plugin concurrently, so assert the reload's own
    // read happened rather than an exact global delta.
    assert!(
        pir::plugins::codemode::settings_load_count() > loads_before,
        "D4: codemode's on_reload re-read settings"
    );
    assert_eq!(
        pir::plugins::codemode::cached_codemode_config().sandbox_memory_bytes,
        Some(32 * 1024 * 1024),
        "D4: the reloaded settings replaced the cached codemode config"
    );
}

// ---------------------------------------------------------------------------
// D4 (c) — a failing plugin `on_reload` never aborts the reload
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn session_reload_survives_a_failing_plugin_on_reload() {
    let temp = TempDir::new("reload-failing-plugin");
    let failing = ReloadRecorder::new("w14e-failing-plugin", true);
    let healthy = ReloadRecorder::new("w14e-healthy-plugin", false);
    let session = AgentSession::new_with_plugins(
        session_config(&temp.0, None),
        vec![
            Arc::clone(&failing) as Arc<dyn AgentPlugin>,
            Arc::clone(&healthy) as Arc<dyn AgentPlugin>,
        ],
    );

    session
        .reload_plugins()
        .await
        .expect("one plugin's on_reload failure must not abort the reload");

    assert_eq!(failing.reloads(), 1, "the failing plugin's hook ran");
    assert_eq!(
        healthy.reloads(),
        1,
        "the plugin AFTER the failing one still reloaded"
    );
    assert_eq!(
        healthy.shutdown_reloads(),
        1,
        "the native teardown event still reached every plugin"
    );
    assert!(
        healthy.registrations() >= 2,
        "the registry rebuild still ran after the failure, got {}",
        healthy.registrations()
    );
}

// ---------------------------------------------------------------------------
// D4 (d) — the JS extension reload still runs (through the same host entry
// point), exactly once, and the native-only session seam does not reload it
// ---------------------------------------------------------------------------

#[cfg(feature = "extensions")]
fn extension_config(cwd: &Path) -> pir::extensions::ExtensionConfig {
    pir::extensions::ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: Some("w14e-model".to_owned()),
        session_id: Some("w14e-session".to_owned()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

#[cfg(feature = "extensions")]
async fn load_js_fixture(temp: &TempDir) -> pir::extensions::ExtensionHandle {
    let path = temp.0.join("reload_marker.ts");
    std::fs::write(
        &path,
        r#"
        export default function (pi) {
            globalThis.__w14e_factory_runs = (globalThis.__w14e_factory_runs ?? 0) + 1;
            pi.on("session_start", (event) => {
                globalThis.__w14e_start_reason = event.reason ?? "none";
            });
        }
        "#,
    )
    .expect("write JS fixture");
    let handle = pir::extensions::ExtensionHandle::start(extension_config(&temp.0))
        .expect("start extension runtime");
    let loaded = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load JS fixture");
    assert!(loaded.errors.is_empty(), "load errors: {:?}", loaded.errors);
    handle
}

#[cfg(feature = "extensions")]
async fn js_start_reason(handle: &pir::extensions::ExtensionHandle) -> serde_json::Value {
    handle
        .eval_json("JSON.stringify(globalThis.__w14e_start_reason ?? null)")
        .await
        .expect("probe the JS runtime")
}

#[cfg(feature = "extensions")]
#[tokio::test(flavor = "current_thread")]
async fn plugin_host_reload_still_reloads_the_js_runtime() {
    let temp = TempDir::new("reload-js");
    // Pin the agent dir: the host build resolves its settings from it (never
    // the real `~/.pi`).
    let _guard = crate::common::AgentDirEnvGuard::scratch("w14e-js-host", None).await;
    let handle = load_js_fixture(&temp).await;
    assert_eq!(
        js_start_reason(&handle).await,
        serde_json::json!(null),
        "no session_start was emitted before the reload"
    );
    let recorder = ReloadRecorder::new("w14e-js-host-recorder", false);
    let host = pir::plugins::PluginHost::build(
        Some(handle.clone()),
        vec![Arc::clone(&recorder) as Arc<dyn AgentPlugin>],
    );

    host.reload().await.expect("composite reload");

    assert_eq!(
        js_start_reason(&handle).await,
        serde_json::json!("reload"),
        "D4: the JS runtime reload lifecycle still runs (ExtensionHandle::reload)"
    );
    assert_eq!(
        recorder.reloads(),
        1,
        "D4: the native plugin hooks ran once in the same composite reload"
    );
}

#[cfg(feature = "extensions")]
#[tokio::test(flavor = "current_thread")]
async fn session_native_reload_leaves_the_js_runtime_to_the_dispatcher() {
    let temp = TempDir::new("reload-js-session");
    let _guard = crate::common::AgentDirEnvGuard::scratch("w14e-js-session", None).await;
    let handle = load_js_fixture(&temp).await;
    let recorder = ReloadRecorder::new("w14e-js-session-recorder", false);
    let session = AgentSession::new_with_plugins(
        session_config(&temp.0, Some(handle.clone())),
        vec![Arc::clone(&recorder) as Arc<dyn AgentPlugin>],
    );

    session
        .reload_plugins()
        .await
        .expect("native session reload seam");

    assert_eq!(recorder.reloads(), 1, "the native hooks ran once");
    assert_eq!(
        js_start_reason(&handle).await,
        serde_json::json!(null),
        "the session seam must not reload the JS runtime: the TUI dispatcher owns \
         that half, so one /reload reloads each face exactly once"
    );

    // The JS face still reloads through its existing driver (the TUI's
    // `ExtRequest::Reload` → `ExtensionHandle::reload`).
    handle.reload().await.expect("JS runtime reload");
    assert_eq!(
        js_start_reason(&handle).await,
        serde_json::json!("reload"),
        "the JS extension reload still happens too"
    );
}

// ---------------------------------------------------------------------------
// Audit §4 #1 — session-level `ctx.reload()` (`AgentSession::reload`):
// extensions + native plugins + tools + skills/prompts/themes reload in place,
// the session and its transcript stay put.
// ---------------------------------------------------------------------------

/// Write one skill file (root-level `.md` with frontmatter).
#[cfg(feature = "extensions")]
fn write_reload_skill(path: &std::path::Path, name: &str, description: &str) {
    std::fs::write(
        path,
        format!("---\nname: {name}\ndescription: {description}\n---\nBody of {name}.\n"),
    )
    .expect("write skill fixture");
}

/// Rewrite the reload-probe extension: a tool (v1/v2 name), the `act-reload`
/// command, and a `resources_discover` contribution of the given paths.
#[cfg(feature = "extensions")]
fn write_reload_probe_extension(
    temp: &TempDir,
    tool_name: &str,
    skill_path: Option<&std::path::Path>,
    prompt_path: Option<&std::path::Path>,
) -> PathBuf {
    let path_strings = |paths: &[Option<&std::path::Path>]| {
        serde_json::Value::Array(
            paths
                .iter()
                .flatten()
                .map(|path| serde_json::Value::String(path.display().to_string()))
                .collect(),
        )
    };
    let skills = path_strings(&[skill_path]);
    let prompts = path_strings(&[prompt_path]);
    let path = temp.0.join("reload_probe.ts");
    let source = format!(
        r#"
        export default function (pi) {{
            globalThis.__reload_probe_factory_runs =
                (globalThis.__reload_probe_factory_runs ?? 0) + 1;
            pi.registerTool({{
                name: "{tool_name}",
                label: "{tool_name} label",
                description: "session reload probe",
                parameters: {{ type: "object", properties: {{}} }},
                execute: async () => ({{ content: [{{ type: "text", text: "ok" }}] }}),
            }});
            pi.registerCommand("act-reload", {{
                handler: async (_args, ctx) => {{ await ctx.reload(); }},
            }});
            pi.on("resources_discover", () => {{
                globalThis.__reload_probe_discoveries =
                    (globalThis.__reload_probe_discoveries ?? 0) + 1;
                return {{ skillPaths: {skills}, promptPaths: {prompts} }};
            }});
            pi.on("session_start", (event) => {{
                globalThis.__reload_probe_start_reason = event.reason ?? "none";
            }});
        }}
        "#
    );
    std::fs::write(&path, source).expect("write reload probe extension");
    path
}

/// A persisted session with one user message (the transcript the reload must
/// preserve).
#[cfg(feature = "extensions")]
fn reload_probe_storage(temp: &TempDir) -> (pir::session_manager::SessionManager, String) {
    let session_dir = temp.0.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let mut manager =
        pir::session_manager::SessionManager::create(&temp.0.display().to_string(), &session_dir)
            .expect("create session");
    let entry_id = manager
        .append_message(pir::ai::types::Message::User(pir::ai::types::UserMessage {
            role: pir::ai::types::UserRole::User,
            content: vec![pir::ai::types::ContentBlock::Text(
                pir::ai::types::TextContent {
                    text: "keep me across the reload".to_owned(),
                    text_signature: None,
                },
            )],
            tools_added: None,
            tools_removed: None,
            timestamp: pir::ai::util::now_ms(),
            sections: None,
        }))
        .expect("append transcript message");
    (manager, entry_id)
}

/// Audit §4 #1: `AgentSession::reload` reloads the JS runtime (factories
/// re-run), the unified registry (a new tool appears), and the contributed
/// skills/prompts — including a CHANGED skill description and a removed
/// prompt — while the session id, leaf and transcript stay untouched.
#[cfg(feature = "extensions")]
#[tokio::test(flavor = "current_thread")]
async fn session_reload_reloads_extensions_tools_and_resources() {
    let temp = TempDir::new("session-reload");
    let _guard = crate::common::AgentDirEnvGuard::scratch("w14e-session-reload", None).await;
    let skill = temp.0.join("alpha-skill.md");
    let prompt = temp.0.join("one-template.md");
    write_reload_skill(&skill, "alpha-skill", "alpha v1");
    std::fs::write(&prompt, "template body one\n").expect("write prompt fixture");
    let extension_path =
        write_reload_probe_extension(&temp, "reload-probe-v1", Some(&skill), Some(&prompt));

    let handle = pir::extensions::ExtensionHandle::start(extension_config(&temp.0))
        .expect("start extension runtime");
    let loaded = handle
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load reload probe");
    assert!(loaded.errors.is_empty(), "load errors: {:?}", loaded.errors);

    let (storage, entry_id) = reload_probe_storage(&temp);
    let mut config = session_config(&temp.0, Some(handle.clone()));
    config.session = Some(storage);
    let session = AgentSession::new_with_plugins(config, Vec::new());
    let session_id = session.session_id().expect("persisted session id");
    let session_path = session.session_path().expect("persisted session path");
    session.push_session_context_for_test().await;

    // First reload installs the v1 contribution.
    let first = session.reload().await.expect("first reload");
    assert!(first.extensions_reloaded, "the JS runtime was rebuilt");
    assert!(first.native_plugins_reloaded, "the native reload ran");
    assert_eq!(
        first.resources.skills, 1,
        "the contributed skill installed: {:?}",
        first.resources
    );
    let (skills, templates) = session.prompt_resources();
    assert_eq!(
        skills
            .iter()
            .find(|skill| skill.name == "alpha-skill")
            .map(|skill| skill.description.as_str()),
        Some("alpha v1"),
        "the v1 skill description is live"
    );
    assert_eq!(
        templates.len(),
        1,
        "the contributed prompt template is live"
    );
    let plugins = session.plugin_host().expect("plugin host");
    assert!(
        session
            .merged_tool_registry_for_test(plugins)
            .0
            .get("reload-probe-v1")
            .is_some(),
        "the v1 tool is registered"
    );

    // The extension changes: new tool name, changed skill description (same
    // path), and no prompt contribution any more.
    write_reload_skill(&skill, "alpha-skill", "alpha v2");
    write_reload_probe_extension(&temp, "reload-probe-v2", Some(&skill), None);
    let second = session.reload().await.expect("second reload");
    assert_eq!(
        second.resources.prompt_templates, 0,
        "the removed prompt contribution was retracted, got {:?}",
        second.resources
    );

    let plugins = session.plugin_host().expect("plugin host");
    let registry = session.merged_tool_registry_for_test(plugins).0;
    assert!(
        registry.get("reload-probe-v2").is_some(),
        "the reloaded extension's new tool is visible to the next run"
    );
    assert!(
        registry.get("reload-probe-v1").is_none(),
        "the previous tool registration was replaced"
    );
    let (skills, templates) = session.prompt_resources();
    assert_eq!(
        skills
            .iter()
            .find(|skill| skill.name == "alpha-skill")
            .map(|skill| skill.description.as_str()),
        Some("alpha v2"),
        "a CHANGED contributed skill takes effect (not shadowed by the old copy)"
    );
    assert!(
        templates.is_empty(),
        "the retracted prompt template is gone: {templates:?}"
    );

    // The session itself never moved.
    assert_eq!(
        session.session_id().as_deref(),
        Some(session_id.as_str()),
        "the reload kept the session"
    );
    assert_eq!(
        session.session_path().as_deref(),
        Some(session_path.as_path()),
        "the reload kept the session file"
    );
    assert_eq!(
        session
            .with_session_manager(|manager| manager.leaf_id().map(str::to_owned))
            .await
            .flatten()
            .as_deref(),
        Some(entry_id.as_str()),
        "the transcript leaf survived the reload"
    );
    assert_eq!(
        session.message_count().await,
        1,
        "the transcript survived the reload"
    );
    let start_reason = handle
        .eval_json("JSON.stringify(globalThis.__reload_probe_start_reason ?? null)")
        .await
        .expect("probe the reloaded runtime");
    assert_eq!(
        start_reason,
        serde_json::json!("reload"),
        "the JS runtime was rebuilt and the extension re-executed on reload"
    );
}

/// Audit §4 #1: an EXTENSION COMMAND's `ctx.reload()` reaches the host reload
/// (recorded action → the idle drain in `dispatch_plugin_command`), so a
/// command can refresh the tool/resource surface without a host `/reload`.
#[cfg(feature = "extensions")]
#[tokio::test(flavor = "current_thread")]
async fn extension_command_ctx_reload_reloads_the_session_resources() {
    let temp = TempDir::new("session-reload-command");
    let _guard = crate::common::AgentDirEnvGuard::scratch("w14e-session-reload-cmd", None).await;
    let skill = temp.0.join("cmd-skill.md");
    write_reload_skill(&skill, "cmd-skill", "command v1");
    let extension_path = write_reload_probe_extension(&temp, "cmd-probe-v1", Some(&skill), None);

    let handle = pir::extensions::ExtensionHandle::start(extension_config(&temp.0))
        .expect("start extension runtime");
    let loaded = handle
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("load reload probe");
    assert!(loaded.errors.is_empty(), "load errors: {:?}", loaded.errors);
    let (storage, _entry_id) = reload_probe_storage(&temp);
    let mut config = session_config(&temp.0, Some(handle.clone()));
    config.session = Some(storage);
    let session = AgentSession::new_with_plugins(config, Vec::new());
    session.push_session_context_for_test().await;

    write_reload_skill(&skill, "cmd-skill", "command v2");
    write_reload_probe_extension(&temp, "cmd-probe-v2", Some(&skill), None);

    let outcome = session
        .dispatch_plugin_command("act-reload", "")
        .await
        .expect("dispatch the extension command");
    assert!(
        outcome.ok,
        "ctx.reload() applied without error: {:?}",
        outcome.error
    );
    let plugins = session.plugin_host().expect("plugin host");
    let registry = session.merged_tool_registry_for_test(plugins).0;
    assert!(
        registry.get("cmd-probe-v2").is_some(),
        "the command-triggered reload installed the new tool"
    );
    assert!(
        registry.get("cmd-probe-v1").is_none(),
        "the old tool registration is gone"
    );
    let (skills, _templates) = session.prompt_resources();
    assert_eq!(
        skills
            .iter()
            .find(|skill| skill.name == "cmd-skill")
            .map(|skill| skill.description.as_str()),
        Some("command v2"),
        "the command-triggered reload refreshed the contributed skill"
    );
    assert_eq!(
        session.message_count().await,
        1,
        "the transcript survived the command-triggered reload"
    );
}
