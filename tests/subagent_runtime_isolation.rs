//! Per-session extension-runtime isolation for the `subagent` builtin (upstream
//! gives every child a full `pi` process — its own extension runtime). The
//! child now spawns a FRESH [`pir::extensions::ExtensionHandle`] from the
//! parent's recorded path plan with the child cwd's config, so:
//!
//! 1. the child's `session_start` dispatch lands on the child runtime and
//!    never reaches the parent's JS handlers;
//! 2. the child cwd's settings snapshot (`pi.getSettings()`) is pushed to the
//!    child runtime and never replaces the parent's snapshot;
//! 3. extension tools execute through the child's own actor;
//! 4. a parent handle without a recorded path plan keeps the pre-isolation
//!    shared-actor fallback (tools still work).
//!
//! The model is faked with a scripted provider (same pattern as
//! `tests/subagent_builtin.rs`); no `~/.pi` configuration is consulted except
//! in the settings test, which redirects `PIR_AGENT_DIR` to a temp dir.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use pir::agent::tool_execution::{AgentTool, ToolContext, ToolRegistry, ToolSessionEnv};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::plugins::{instantiate_builtin, SubagentPlugin};

// ---------------------------------------------------------------------------
// Fakes (mirrors tests/subagent_builtin.rs)
// ---------------------------------------------------------------------------

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

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
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

fn tool_call_block(id: &str, name: &str, arguments: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments
            .as_object()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// The last tool result's first text block for `tool_name` (read from the
/// child's request context on the echo turn).
fn tool_result_text(context: &Context, tool_name: &str) -> Option<String> {
    context
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_name == tool_name => {
                result.content.iter().find_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
            }
            _ => None,
        })
}

/// Responder signature: (call index, request context) -> assistant message.
type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

/// A fake provider that answers every stream with a scripted message.
struct FakeProvider {
    calls: AtomicUsize,
    respond: Responder,
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
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn fake_registry(respond: Responder) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond,
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
            "pir-subagent-iso-{label}-{}-{}",
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

/// Start + load a parent runtime with the extension on its recorded path
/// plan (mirrors `load_extension_runtime` minus host-specific flag/trust
/// plumbing).
async fn start_parent_runtime(cwd: &Path, ext_path: &Path) -> pir::extensions::ExtensionHandle {
    let handle = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("fake-model".to_string()),
        session_id: Some("isolation-test-parent".to_string()),
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

/// Wire the handle into the `subagent` builtin exactly as `PluginHost::build`
/// does, and return the registered Rust tool backend.
fn subagent_tool_with_handle(handle: pir::extensions::ExtensionHandle) -> Arc<dyn AgentTool> {
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

fn first_text_of(result: &pir::extensions::ToolResult) -> String {
    result
        .content
        .iter()
        .find(|item| item.kind == "text")
        .map(|item| item.text.clone())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 1. Child `session_start` lands on the child runtime, never the parent's
// ---------------------------------------------------------------------------

/// The parent runtime has already fired its own `session_start` (counter = 1).
/// The child must count EXACTLY its own `session_start` on its own runtime
/// (`SESSION_STARTS:1`); a shared actor would answer `2`. The parent's
/// counter must still read 1 afterwards — the child's lifecycle never
/// reached the parent's JS handlers.
#[tokio::test(flavor = "current_thread")]
async fn child_session_start_lands_on_the_child_runtime_not_the_parent() {
    // The runtime build counter is process-global: serialize the tests that
    // spawn child runtimes so no concurrent build can move it mid-assertion.
    let _build_guard = BUILD_COUNT_LOCK.lock().await;
    let temp = TempDir::new("session-start");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: probe runner\ntools: probe\n",
        "Use the probe tool.",
    );
    let ext_path = temp.0.join("ext.ts");
    std::fs::write(
        &ext_path,
        r#"
        let sessionStarts = 0;
        export default function (pi) {
            pi.on("session_start", async () => { sessionStarts += 1; });
            pi.registerTool({
                name: "probe",
                description: "reports THIS runtime's session_start count",
                parameters: { type: "object" },
                execute: async () => ({
                    content: [{ type: "text", text: "SESSION_STARTS:" + sessionStarts }],
                }),
            });
        }
    "#,
    )
    .expect("write extension");

    let parent = start_parent_runtime(&temp.0, &ext_path).await;
    // The parent host's own lifecycle dispatch (AgentSession fires this
    // before its first prompt): the parent counter becomes 1.
    let mut payload = serde_json::json!({ "type": "session_start" });
    parent
        .emit("session_start", &mut payload)
        .await
        .expect("parent session_start");

    // Child: turn 0 calls the probe tool, turn 1 echoes its output.
    let registry = fake_registry(Arc::new(|index, context| {
        if index == 0 {
            assistant(
                vec![tool_call_block("call-1", "probe", serde_json::json!({}))],
                StopReason::ToolUse,
            )
        } else {
            let observed =
                tool_result_text(context, "probe").unwrap_or_else(|| "MISSING".to_owned());
            assistant(vec![text(&format!("ECHO:{observed}"))], StopReason::Stop)
        }
    }));

    let tool = subagent_tool_with_handle(parent.clone());
    let env = session_env(registry, Some(temp.0.join("agents")));
    let tool_ctx = tool_context(&temp.0, env);
    let builds_before_child = pir::extensions::runtime::js_runtime_build_count();
    let result = tool
        .run(
            &serde_json::json!({ "agent": "scout", "task": "run the probe" }),
            &tool_ctx,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    // The child's OWN runtime: fresh module (counter 0) + exactly its own
    // session_start. On the pre-isolation shared actor this would read 2
    // (the parent's session_start already counted there).
    assert!(
        result.content.contains("ECHO:SESSION_STARTS:1"),
        "child probe: {}",
        result.content
    );
    // The parent plan carries an on-disk JS extension, so the child spawn
    // must still construct a fresh QuickJS runtime.
    assert!(
        pir::extensions::runtime::js_runtime_build_count() > builds_before_child,
        "an on-disk JS extension must start a QuickJS runtime for the child"
    );

    // The parent runtime is untouched: still its own 1 — the child's
    // session_start never reached the parent's JS handlers.
    let parent_probe = parent
        .call_tool("probe", "parent-probe", &serde_json::json!({}))
        .await
        .expect("parent probe");
    assert_eq!(first_text_of(&parent_probe), "SESSION_STARTS:1");
}

// ---------------------------------------------------------------------------
// 2. Child cwd settings snapshot never replaces the parent's getSettings
// ---------------------------------------------------------------------------

/// Serialize env mutation (process-global) within this test binary.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// `js_runtime_build_count()` is process-global: tests that assert on it must
/// not run concurrently with another test that builds a child runtime.
static BUILD_COUNT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Restores the redirected env vars on drop (panic-safe).
struct EnvGuard {
    agent_dir: Option<std::ffi::OsString>,
    trust: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.agent_dir.take() {
            Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
            None => std::env::remove_var("PIR_AGENT_DIR"),
        }
        match self.trust.take() {
            Some(value) => std::env::set_var("PIR_TRUST_PROJECT", value),
            None => std::env::remove_var("PIR_TRUST_PROJECT"),
        }
    }
}

/// The parent runtime's `pi.getSettings()` snapshot is `{"globalMarker": ...}`
/// (pushed by the parent host). The child runs with a DIFFERENT cwd whose
/// trusted project settings add `childMarker`; the child's own runtime must
/// report it, and the parent's snapshot must be unchanged afterwards (the
/// child build's `update_settings` used to land on the shared actor and
/// replace the parent's snapshot).
#[tokio::test(flavor = "current_thread")]
async fn child_cwd_settings_snapshot_does_not_touch_the_parent_runtime() {
    // The runtime build counter is process-global: serialize the tests that
    // spawn child runtimes so no concurrent build can move it mid-assertion.
    let _build_guard = BUILD_COUNT_LOCK.lock().await;
    let _env = {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let agent_dir = std::env::var_os("PIR_AGENT_DIR");
        let trust = std::env::var_os("PIR_TRUST_PROJECT");
        EnvGuard {
            agent_dir,
            trust,
            _lock: lock,
        }
    };

    let temp = TempDir::new("settings");
    let global_dir = temp.0.join("global");
    std::fs::create_dir_all(&global_dir).expect("global agent dir");
    std::fs::write(
        global_dir.join("settings.json"),
        r#"{ "globalMarker": "global-scope" }"#,
    )
    .expect("global settings");
    let parent_cwd = temp.0.join("parent");
    let child_cwd = temp.0.join("child");
    std::fs::create_dir_all(child_cwd.join(".pi")).expect("child project dir");
    std::fs::write(
        child_cwd.join(".pi").join("settings.json"),
        r#"{ "childMarker": "child-cwd" }"#,
    )
    .expect("child project settings");
    std::env::set_var("PIR_AGENT_DIR", &global_dir);
    std::env::set_var("PIR_TRUST_PROJECT", "1");

    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: settings probe runner\ntools: settings-probe\n",
        "Use the settings-probe tool.",
    );
    let ext_path = temp.0.join("ext.ts");
    std::fs::write(
        &ext_path,
        r#"
        export default function (pi) {
            pi.registerTool({
                name: "settings-probe",
                description: "returns JSON.stringify(pi.getSettings())",
                parameters: { type: "object" },
                execute: async () => ({
                    content: [{ type: "text", text: JSON.stringify(pi.getSettings()) }],
                }),
            });
        }
    "#,
    )
    .expect("write extension");

    let parent = start_parent_runtime(&parent_cwd, &ext_path).await;
    // The parent host's settings push (AgentSession::build for the parent
    // cwd; the parent project has no settings, so this is the global scope).
    parent.update_settings(serde_json::json!({ "globalMarker": "global-scope" }));

    let registry = fake_registry(Arc::new(|index, context| {
        if index == 0 {
            assistant(
                vec![tool_call_block(
                    "call-1",
                    "settings-probe",
                    serde_json::json!({}),
                )],
                StopReason::ToolUse,
            )
        } else {
            let observed =
                tool_result_text(context, "settings-probe").unwrap_or_else(|| "MISSING".to_owned());
            assistant(vec![text(&format!("ECHO:{observed}"))], StopReason::Stop)
        }
    }));

    let tool = subagent_tool_with_handle(parent.clone());
    let env = session_env(registry, Some(temp.0.join("agents")));
    // Single-mode `cwd` override: the child runs in a DIFFERENT directory.
    let tool_ctx = tool_context(&parent_cwd, env);
    let result = tool
        .run(
            &serde_json::json!({
                "agent": "scout",
                "task": "probe the settings",
                "cwd": child_cwd.display().to_string(),
            }),
            &tool_ctx,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);

    // The child's own runtime saw the CHILD cwd's merged settings snapshot.
    assert!(
        result.content.contains("childMarker") && result.content.contains("child-cwd"),
        "child settings probe: {}",
        result.content
    );

    // The parent's snapshot was NOT replaced by the child's: still the
    // parent push exactly, no childMarker anywhere in it.
    let parent_probe = parent
        .call_tool("settings-probe", "parent-probe", &serde_json::json!({}))
        .await
        .expect("parent settings probe");
    let parent_settings = first_text_of(&parent_probe);
    assert!(
        parent_settings.contains("globalMarker") && parent_settings.contains("global-scope"),
        "parent settings probe: {parent_settings}"
    );
    assert!(
        !parent_settings.contains("childMarker"),
        "parent settings polluted by the child cwd: {parent_settings}"
    );
}

// ---------------------------------------------------------------------------
// 3. Extension tools execute through the child's own actor
// ---------------------------------------------------------------------------

/// The parent executes the counter tool first (`COUNT:1` on the parent
/// runtime). The child then executes the SAME tool: on the child's own fresh
/// runtime the counter restarts (`COUNT:1`); a shared actor would answer
/// `COUNT:2`. Both runtimes stay usable afterwards.
#[tokio::test(flavor = "current_thread")]
async fn child_extension_tool_runs_through_the_new_child_runtime() {
    // The runtime build counter is process-global: serialize the tests that
    // spawn child runtimes so no concurrent build can move it mid-assertion.
    let _build_guard = BUILD_COUNT_LOCK.lock().await;
    let temp = TempDir::new("new-actor-tool");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: counter tool user\ntools: boot-tag\n",
        "Use the boot-tag tool.",
    );
    let ext_path = temp.0.join("ext.ts");
    std::fs::write(
        &ext_path,
        r#"
        let count = 0;
        export default function (pi) {
            pi.registerTool({
                name: "boot-tag",
                description: "increments a per-runtime counter and reports it",
                parameters: { type: "object" },
                execute: async () => {
                    count += 1;
                    return { content: [{ type: "text", text: "COUNT:" + count }] };
                },
            });
        }
    "#,
    )
    .expect("write extension");

    let parent = start_parent_runtime(&temp.0, &ext_path).await;
    let parent_tag = parent
        .call_tool("boot-tag", "parent-tag", &serde_json::json!({}))
        .await
        .expect("parent tool call");
    assert_eq!(first_text_of(&parent_tag), "COUNT:1");

    let registry = fake_registry(Arc::new(|index, context| {
        if index == 0 {
            assistant(
                vec![tool_call_block("call-1", "boot-tag", serde_json::json!({}))],
                StopReason::ToolUse,
            )
        } else {
            let observed =
                tool_result_text(context, "boot-tag").unwrap_or_else(|| "MISSING".to_owned());
            assistant(vec![text(&format!("ECHO:{observed}"))], StopReason::Stop)
        }
    }));

    let tool = subagent_tool_with_handle(parent.clone());
    let env = session_env(registry, Some(temp.0.join("agents")));
    let tool_ctx = tool_context(&temp.0, env);
    let result = tool
        .run(
            &serde_json::json!({ "agent": "scout", "task": "call boot-tag" }),
            &tool_ctx,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("ECHO:COUNT:1"),
        "child tool ran on the parent's runtime instead of a fresh one: {}",
        result.content
    );

    // The parent runtime is untouched and still usable.
    let parent_tag = parent
        .call_tool("boot-tag", "parent-tag-2", &serde_json::json!({}))
        .await
        .expect("parent tool call after the child run");
    assert_eq!(first_text_of(&parent_tag), "COUNT:2");
}

// ---------------------------------------------------------------------------
// 4. Fallback: a parent handle without a recorded path plan shares the actor
// ---------------------------------------------------------------------------

/// A directly started handle (no `set_path_plan`) has no extension path plan
/// to rebuild: the child falls back to sharing the parent's handle, and the
/// extension tool still executes (pre-isolation behavior preserved).
#[tokio::test(flavor = "current_thread")]
async fn parent_without_path_plan_falls_back_to_the_shared_handle() {
    // The runtime build counter is process-global: serialize the tests that
    // spawn child runtimes so no concurrent build can move it mid-assertion.
    let _build_guard = BUILD_COUNT_LOCK.lock().await;
    let temp = TempDir::new("fallback");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: shared tool user\ntools: shared-tool\n",
        "Use the shared-tool.",
    );
    let ext_path = temp.0.join("ext.ts");
    std::fs::write(
        &ext_path,
        r#"
        export default function (pi) {
            pi.registerTool({
                name: "shared-tool",
                description: "answers through whatever runtime holds it",
                parameters: { type: "object" },
                execute: async () => ({
                    content: [{ type: "text", text: "SHARED_OK" }],
                }),
            });
        }
    "#,
    )
    .expect("write extension");

    // No `set_path_plan`: the recorded plan is empty → fallback path.
    let parent = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("fake-model".to_string()),
        session_id: Some("fallback-parent".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime start");
    let load = parent.load(&[ext_path]).await.expect("load extension");
    assert!(load.errors.is_empty(), "{:?}", load.errors);

    let registry = fake_registry(Arc::new(|index, context| {
        if index == 0 {
            assistant(
                vec![tool_call_block(
                    "call-1",
                    "shared-tool",
                    serde_json::json!({}),
                )],
                StopReason::ToolUse,
            )
        } else {
            let observed =
                tool_result_text(context, "shared-tool").unwrap_or_else(|| "MISSING".to_owned());
            assistant(vec![text(&format!("ECHO:{observed}"))], StopReason::Stop)
        }
    }));

    let tool = subagent_tool_with_handle(parent.clone());
    let env = session_env(registry, Some(temp.0.join("agents")));
    let tool_ctx = tool_context(&temp.0, env);
    let result = tool
        .run(
            &serde_json::json!({ "agent": "scout", "task": "call the shared tool" }),
            &tool_ctx,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("ECHO:SHARED_OK"),
        "fallback shared-actor tool execution failed: {}",
        result.content
    );
}

// ---------------------------------------------------------------------------
// 5. A builtin-only plan does not start QuickJS per child
// ---------------------------------------------------------------------------

/// A parent whose recorded plan contains only `builtin:<name>` entries has no
/// on-disk JS path. The child runtime must be built plan-only (shared state +
/// path plan, no actor): the pre-fix `ExtensionHandle::start` constructed the
/// full QuickJS runtime + prelude for every child even though the load list
/// was empty.
#[tokio::test(flavor = "current_thread")]
async fn builtin_only_parent_does_not_build_quickjs_for_the_child_runtime() {
    // The runtime build counter is process-global: serialize the tests that
    // spawn child runtimes so no concurrent build can move it mid-assertion.
    let _build_guard = BUILD_COUNT_LOCK.lock().await;
    let temp = TempDir::new("builtin-only-plan");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: plain child\n",
        "Answer the task.",
    );

    let parent =
        pir::extensions::ExtensionHandle::start_plan_only(pir::extensions::ExtensionConfig {
            cwd: temp.0.clone(),
            mode: "headless".to_string(),
            has_ui: false,
            model: Some("fake-model".to_string()),
            session_id: Some("builtin-only-parent".to_string()),
            system_prompt: String::new(),
            is_idle: true,
            limits: Some(Default::default()),
        });
    assert!(
        !parent.has_js_runtime(),
        "the fixture parent must be plan-only"
    );
    // The default CLI extension plan: builtins only, no on-disk JS path.
    parent.set_path_plan(vec![PathBuf::from("builtin:subagent")]);

    let registry = fake_registry(Arc::new(|_, _| {
        assistant(vec![text("CHILD_OK")], StopReason::Stop)
    }));
    let env = session_env(registry, Some(temp.0.join("agents")));
    let tool_ctx = tool_context(&temp.0, env);
    let tool = subagent_tool_with_handle(parent.clone());

    let builds_before = pir::extensions::runtime::js_runtime_build_count();
    let result = tool
        .run(
            &serde_json::json!({ "agent": "scout", "task": "no quickjs please" }),
            &tool_ctx,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "CHILD_OK");
    assert_eq!(
        pir::extensions::runtime::js_runtime_build_count(),
        builds_before,
        "a builtin-only child plan must not construct the QuickJS runtime"
    );
}
